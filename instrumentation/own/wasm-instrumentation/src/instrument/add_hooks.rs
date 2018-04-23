use ast::{FunctionType, GlobalType, Idx, Label, Limits, Local, Memarg, MemoryType, Mutability, ValType, ValType::*};
use ast::highlevel::{Code, Expr, Function, Instr, Instr::*, InstrGroup, InstrGroup::*, Memory, Module};
use js_codegen::append_mangled_tys;
use serde_json;
use static_info::*;
use std::collections::{HashMap, HashSet};
use std::mem::{discriminant, Discriminant};
use super::convert_i64::{convert_i64_instr, convert_i64_type};

fn add_hook(module: &mut Module, name: impl Into<String>, arg_tys_: &[ValType]) -> Idx<Function> {
    // prepend two I32 for (function idx, instr idx)
    let mut arg_tys = vec![I32, I32];
    arg_tys.extend(arg_tys_.iter()
        // and expand i64 to a tuple of (i32, i32) since there is no JS interop for i64
        .flat_map(convert_i64_type));

    module.add_function_import(
        // hooks do not return anything
        FunctionType::new(arg_tys, vec![]),
        "hooks".into(),
        name.into())
}

// TODO put this in the MonomorphicHookMap.add() function instead
/// specialized version form of the above for monomorphic instructions
fn add_hook_from_instr(module: &mut Module, instr: &Instr) -> (Discriminant<Instr>, Idx<Function>) {
    println!("{}", instr.to_js_hook());
    (discriminant(instr), add_hook(module, instr.to_instr_name(), &match instr.group() {
        Const(ty) => vec![ty],
        Unary { input_ty, result_ty } => vec![input_ty, result_ty],
        Binary { first_ty, second_ty, result_ty } => vec![first_ty, second_ty, result_ty],
        // for address, offset and alignment
        MemoryLoad(ty, _) => vec![I32, I32, I32, ty],
        MemoryStore(ty, _) => vec![I32, I32, I32, ty],
        Other => unreachable!("function should be only called for \"grouped\" instructions"),
    }))
}

struct PolymorphicHookMap(HashMap<(Discriminant<Instr>, Vec<ValType>), Idx<Function>>);

impl PolymorphicHookMap {
    pub fn new() -> Self {
        PolymorphicHookMap(HashMap::new())
    }
    pub fn add(&mut self, module: &mut Module, instr: Instr, non_poly_args: &[ValType], tys: &[Vec<ValType>]) {
        for tys in tys {
            println!("{}", instr.to_poly_js_hook(tys.as_slice()));
            let hook_name = append_mangled_tys(instr.to_instr_name(), tys.as_slice());
            let hook_idx = add_hook(module, hook_name, &[non_poly_args, tys.as_slice()].concat());
            self.0.insert(
                (discriminant(&instr), tys.clone()),
                hook_idx);
        }
    }
    pub fn get_call(&self, instr: &Instr, tys: Vec<ValType>) -> Instr {
        let error = format!("no hook was added for {} with types {:?}", instr.to_instr_name(), tys);
        Call(*self.0
            .get(&(discriminant(instr), tys))
            .expect(&error))
    }
}

/// helper function to save top locals.len() values into locals with the given index
/// types of locals must match stack, not enforced by this function!
fn save_stack_to_locals(locals: &[Idx<Local>]) -> Vec<Instr> {
    let mut instrs = Vec::new();
    // copy stack values into locals
    for &local in locals.iter().skip(1).rev() {
        instrs.push(SetLocal(local));
    }
    // optimization: for first local on the stack / last one saved use tee_local instead of set_local + get_local
    for &local in locals.iter().next() {
        instrs.push(TeeLocal(local));
    }
    // and restore (saving has removed them from the stack)
    for &local in locals.iter().skip(1) {
        instrs.push(GetLocal(local));
    }
    return instrs;
}

// TODO why not have a slice of tuples (Idx, ValType)?
fn restore_locals_with_i64_handling(locals: &[Idx<Local>], local_tys: &[ValType]) -> Vec<Instr> {
    assert_eq!(locals.len(), local_tys.len());

    let mut instrs = Vec::new();
    for (&local, &ty) in locals.iter().zip(local_tys.iter()) {
        instrs.append(&mut convert_i64_instr(GetLocal(local), ty));
    }
    return instrs;
}

/// also keeps instruction index, needed later for End hooks
#[derive(Debug)]
enum Begin {
    // TODO include abstract block stack (i.e. Vec<ValType>) into this enum for
    // a) drop/select monomorphization
    // b) type checking
    // c) statically figuring out implicit drops during br/br_if/br_table
    // function begins correspond to no actual instruction, so no instruction index
    Function,
    Block(usize),
    Loop(usize),
    If(usize),
    Else(usize),
}

fn label_to_instr_idx(begin_stack: &[Begin], label: Idx<Label>) -> usize {
    let target_block = begin_stack.iter()
        .rev().nth(label.0)
        .expect(&format!("cannot resolve target for {:?}", label));
    match *target_block {
        Begin::Function => 0,
        Begin::Loop(begin_iidx) => begin_iidx,
        // FIXME if/else/block (forward jump, needs forward scanning for End)
        Begin::If(i) | Begin::Else(i) | Begin::Block(i) => i
    }
}

pub fn add_hooks(module: &mut Module) -> Option<String> {
    // export the table for the JS code to translate table indices -> function indices
    for table in &mut module.tables {
        if let None = table.export {
            table.export = Some("table".into());
        }
    }

    let mut static_info: ModuleInfo = (&*module).into();

    /* add hooks (imported functions, provided by the analysis in JavaScript) */

    // polymorphic hooks:
    // - 1 instruction : N hooks
    // - instruction can take stack arguments/produce results of several types
    // - we need to "monomorphize", i.e., create one hook per occurring polymorphic type
    let mut polymorphic_hooks = PolymorphicHookMap::new();

    // collect some info, necessary for monomorphization of polymorphic hooks
    let (mut unique_arg_tys, mut unique_result_tys): (Vec<Vec<ValType>>, Vec<Vec<ValType>>) = module.functions.iter()
        .map(|func| (func.type_.params.clone(), func.type_.results.clone()))
        .unzip();
    unique_result_tys.sort();
    unique_result_tys.dedup();
    unique_arg_tys.sort();
    unique_arg_tys.dedup();

    // returns
    polymorphic_hooks.add(module, Return, &[], unique_result_tys.as_slice());

    // locals and globals
    let primitive_tys = &[vec![I32], vec![I64], vec![F32], vec![F64]];
    polymorphic_hooks.add(module, GetLocal(0.into()), &[I32], primitive_tys);
    polymorphic_hooks.add(module, SetLocal(0.into()), &[I32], primitive_tys);
    polymorphic_hooks.add(module, TeeLocal(0.into()), &[I32], primitive_tys);
    polymorphic_hooks.add(module, GetGlobal(0.into()), &[I32], primitive_tys);
    polymorphic_hooks.add(module, SetGlobal(0.into()), &[I32], primitive_tys);

    // calls
    polymorphic_hooks.add(module, Call(0.into()), &[I32], unique_arg_tys.as_slice()); // I32 = target func idx
    polymorphic_hooks.add(module, CallIndirect(FunctionType::new(vec![], vec![]), 0.into()), &[I32], unique_arg_tys.as_slice()); // I32 = target table idx
    // manually add call_post hook since it does not directly correspond to an instruction
    let call_result_hooks: HashMap<&[ValType], Idx<Function>> = unique_result_tys.iter()
        .map(|tys| {
            let tys = tys.as_slice();
            (tys, add_hook(module, append_mangled_tys("call_result".into(), tys), tys))
        }).collect();

    // monomorphic hooks:
    // - 1 hook : 1 instruction
    // - argument/result types are directly determined from the instruction itself
    let if_hook = add_hook(module, "if_", &[/* condition */ I32]);
    // [I32, I32] for label and target instruction index (determined statically)
    let br_hook = add_hook(module, "br", &[I32, I32]);
    let br_if_hook = add_hook(module, "br_if", &[/* condition */ I32, /* target label and instr */ I32, I32]);
    let br_table_hook = add_hook(module, "br_table", &[/* br_table_info_idx */ I32, /* table_idx */ I32]);

    // all end hooks also give the instruction index of the corresponding begin (except for functions,
    // where it implicitly is -1 anyway)
    let begin_function_hook = add_hook(module, "begin_function", &[]);
    let end_function_hook = add_hook(module, "end_function", &[]);
    let begin_block_hook = add_hook(module, "begin_block", &[]);
    let end_block_hook = add_hook(module, "end_block", &[I32]);
    let begin_loop_hook = add_hook(module, "begin_loop", &[]);
    let end_loop_hook = add_hook(module, "end_loop", &[I32]);
    let begin_if_hook = add_hook(module, "begin_if", &[]);
    let end_if_hook = add_hook(module, "end_if", &[I32]);
    let begin_else_hook = add_hook(module, "begin_else", &[]);
    let end_else_hook = add_hook(module, "end_else", &[I32]);

    let nop_hook = add_hook(module, "nop", &[]);
    let unreachable_hook = add_hook(module, "unreachable", &[]);

    // drop and select are polymorphic (even "worse" than return and call: we need to type the
    // stack in order to find out the argument types) -> FIXME for now, just ignore the values
    let drop_hook = add_hook(module, "drop", &[]);
    let select_hook = add_hook(module, "select", &[I32]);

    let current_memory_hook = add_hook(module, "current_memory", &[I32]);
    let grow_memory_hook = add_hook(module, "grow_memory", &[I32, I32]);

    // TODO make this a struct of its own, similar to PolymorphicHookMap
    let monomorphic_hook_call = {
        let monomorphic_hooks: HashMap<Discriminant<Instr>, Idx<Function>> = [
            I32Const(0),
            I64Const(0),
            F32Const(0.0),
            F64Const(0.0),

            // Unary
            I32Eqz, I64Eqz,
            I32Clz, I32Ctz, I32Popcnt,
            I64Clz, I64Ctz, I64Popcnt,
            F32Abs, F32Neg, F32Ceil, F32Floor, F32Trunc, F32Nearest, F32Sqrt,
            F64Abs, F64Neg, F64Ceil, F64Floor, F64Trunc, F64Nearest, F64Sqrt,
            I32WrapI64,
            I32TruncSF32, I32TruncUF32,
            I32TruncSF64, I32TruncUF64,
            I64ExtendSI32, I64ExtendUI32,
            I64TruncSF32, I64TruncUF32,
            I64TruncSF64, I64TruncUF64,
            F32ConvertSI32, F32ConvertUI32,
            F32ConvertSI64, F32ConvertUI64,
            F32DemoteF64,
            F64ConvertSI32, F64ConvertUI32,
            F64ConvertSI64, F64ConvertUI64,
            F64PromoteF32,
            I32ReinterpretF32,
            I64ReinterpretF64,
            F32ReinterpretI32,
            F64ReinterpretI64,

            // Binary
            I32Eq, I32Ne, I32LtS, I32LtU, I32GtS, I32GtU, I32LeS, I32LeU, I32GeS, I32GeU,
            I64Eq, I64Ne, I64LtS, I64LtU, I64GtS, I64GtU, I64LeS, I64LeU, I64GeS, I64GeU,
            F32Eq, F32Ne, F32Lt, F32Gt, F32Le, F32Ge,
            F64Eq, F64Ne, F64Lt, F64Gt, F64Le, F64Ge,
            I32Add, I32Sub, I32Mul, I32DivS, I32DivU, I32RemS, I32RemU, I32And, I32Or, I32Xor, I32Shl, I32ShrS, I32ShrU, I32Rotl, I32Rotr,
            I64Add, I64Sub, I64Mul, I64DivS, I64DivU, I64RemS, I64RemU, I64And, I64Or, I64Xor, I64Shl, I64ShrS, I64ShrU, I64Rotl, I64Rotr,
            F32Add, F32Sub, F32Mul, F32Div, F32Min, F32Max, F32Copysign,
            F64Add, F64Sub, F64Mul, F64Div, F64Min, F64Max, F64Copysign,

            // Memory
            I32Load(Memarg::default()), I32Load8S(Memarg::default()), I32Load8U(Memarg::default()), I32Load16S(Memarg::default()), I32Load16U(Memarg::default()),
            I64Load(Memarg::default()), I64Load8S(Memarg::default()), I64Load8U(Memarg::default()), I64Load16S(Memarg::default()), I64Load16U(Memarg::default()), I64Load32S(Memarg::default()), I64Load32U(Memarg::default()),
            F32Load(Memarg::default()),
            F64Load(Memarg::default()),
            I32Store(Memarg::default()), I32Store8(Memarg::default()), I32Store16(Memarg::default()),
            I64Store(Memarg::default()), I64Store8(Memarg::default()), I64Store16(Memarg::default()), I64Store32(Memarg::default()),
            F32Store(Memarg::default()),
            F64Store(Memarg::default()),
        ].into_iter()
            .map(|i| add_hook_from_instr(module, i))
            .collect();

        move |instr: &Instr| -> Instr {
            Call(*monomorphic_hooks
                .get(&discriminant(instr))
                .expect(&format!("no hook was added for instruction {}", instr.to_instr_name())))
        }
    };

    /* add call to hooks: setup code that copies the returned value, instruction location, call */
    // NOTE we do not need to filter out functions since all hooks are imports and thus won't have
    // Code to instrument anyway...
    for (fidx, function) in module.functions() {
        // only instrument non-imported functions
        if function.code.is_none() {
            continue;
        }

        // move body out of function, so that function is not borrowed during iteration over the original body
        let original_body = {
            let dummy_body = Vec::new();
            ::std::mem::replace(&mut function.code.as_mut().unwrap().body, dummy_body)
        };

        // allocate new instrumented body (i.e., do not modify in-place), since there are too many insertions anyway
        // there are at least 3 new instructions per original one (2 const for location + 1 hook call)
        let mut instrumented_body = Vec::with_capacity(4 * original_body.len());

        // TODO rename block_stack
        let mut begin_stack = vec![Begin::Function];

        // add function_begin hook...
        instrumented_body.extend_from_slice(&[
            I32Const(fidx.0 as i32),
            // ...which does not correspond to any instruction, so take -1 as instruction index
            I32Const(-1),
            Call(begin_function_hook)
        ]);

        for (iidx, instr) in original_body.into_iter().enumerate() {
            let location = (I32Const(fidx.0 as i32), I32Const(iidx as i32));
            instrumented_body.append(&mut
                match (instr.group(), instr.clone()) {
                    (_, Block(_)) => {
                        begin_stack.push(Begin::Block(iidx));
                        vec![
                            instr,
                            location.0,
                            location.1,
                            Call(begin_block_hook),
                        ]
                    }
                    (_, Loop(_)) => {
                        begin_stack.push(Begin::Loop(iidx));
                        vec![
                            instr,
                            location.0,
                            location.1,
                            Call(begin_loop_hook),
                        ]
                    }
                    (_, If(_)) => {
                        begin_stack.push(Begin::If(iidx));

                        let condition_tmp = function.add_fresh_local(I32);

                        vec![
                            // if_ hook for the condition (always executed on either branch)
                            TeeLocal(condition_tmp),
                            location.0.clone(),
                            location.1.clone(),
                            GetLocal(condition_tmp),
                            Call(if_hook),
                            // actual if block start
                            instr,
                            // begin hook (not executed when condition implies else branch)
                            location.0,
                            location.1,
                            Call(begin_if_hook),
                        ]
                    }
                    (_, Else) => {
                        let begin = begin_stack.pop()
                            .expect(&format!("invalid begin/end nesting in function {}!", fidx.0));
                        if let Begin::If(begin_iidx) = begin {
                            begin_stack.push(Begin::Else(iidx));
                            vec![
                                location.0.clone(),
                                location.1.clone(),
                                I32Const(begin_iidx as i32),
                                Call(end_else_hook),
                                instr,
                                location.0,
                                location.1,
                                Call(begin_else_hook),
                            ]
                        } else {
                            unreachable!("else instruction should end if block, but was {:?}", begin);
                        }
                    }
                    (_, End) => {
                        let begin = begin_stack.pop()
                            .expect(&format!("invalid begin/end nesting in function {}!", fidx.0));

                        let mut instrs = vec![
                            location.0,
                            location.1,
                        ];
                        instrs.append(&mut match begin {
                            Begin::Function => vec![Call(end_function_hook)],
                            Begin::Block(begin_iidx) => vec![I32Const(begin_iidx as i32), Call(end_block_hook)],
                            Begin::Loop(begin_iidx) => vec![I32Const(begin_iidx as i32), Call(end_loop_hook)],
                            Begin::If(begin_iidx) => vec![I32Const(begin_iidx as i32), Call(end_if_hook)],
                            Begin::Else(begin_iidx) => vec![I32Const(begin_iidx as i32), Call(end_else_hook)],
                        });
                        instrs.push(instr);
                        instrs
                    }
                    (_, Nop) => vec![
                        instr,
                        location.0,
                        location.1,
                        Call(nop_hook),
                    ],
                    (_, Unreachable) => vec![
                        instr,
                        location.0,
                        location.1,
                        Call(unreachable_hook),
                    ],
                    // TODO monomorphize value
                    (_, Drop) => vec![
                        instr,
                        location.0,
                        location.1,
                        Call(drop_hook),
                    ],
                    (_, Select) => {
                        let cond_tmp = function.add_fresh_local(I32);

                        // TODO monomorphize for first and second argument

                        vec![
                            TeeLocal(cond_tmp),
                            instr,
                            location.0,
                            location.1,
                            GetLocal(cond_tmp),
                            Call(select_hook),
                        ]
                    }
                    (_, CurrentMemory(_ /* memory idx == 0 in WASM version 1 */)) => {
                        let result_tmp = function.add_fresh_local(I32);
                        vec![
                            instr,
                            TeeLocal(result_tmp),
                            location.0,
                            location.1,
                            GetLocal(result_tmp),
                            Call(current_memory_hook)
                        ]
                    }
                    (_, GrowMemory(_ /* memory idx == 0 in WASM version 1 */)) => {
                        let input_tmp = function.add_fresh_local(I32);
                        let result_tmp = function.add_fresh_local(I32);
                        vec![
                            TeeLocal(input_tmp),
                            instr,
                            TeeLocal(result_tmp),
                            location.0,
                            location.1,
                            GetLocal(input_tmp),
                            GetLocal(result_tmp),
                            Call(grow_memory_hook)
                        ]
                    }
                    (_, GetLocal(local_idx)) | (_, SetLocal(local_idx)) | (_, TeeLocal(local_idx)) => {
                        let local_ty = function.local_type(local_idx);
                        let mut instrs = vec![
                            instr.clone(),
                            location.0,
                            location.1,
                            I32Const(local_idx.0 as i32),
                        ];
                        instrs.append(&mut convert_i64_instr(GetLocal(local_idx), local_ty));
                        instrs.push(polymorphic_hooks.get_call(&instr, vec![local_ty]));
                        instrs
                    }
                    (_, GetGlobal(global_idx)) | (_, SetGlobal(global_idx)) => {
                        let global_ty = static_info.globals[global_idx.0];
                        let mut instrs = vec![
                            instr.clone(),
                            location.0,
                            location.1,
                            I32Const(global_idx.0 as i32),
                        ];
                        instrs.append(&mut convert_i64_instr(GetGlobal(global_idx), global_ty));
                        instrs.push(polymorphic_hooks.get_call(&instr, vec![global_ty]));
                        instrs
                    }
                    (_, Return) => {
                        let result_tys = function.type_.results.clone();
                        let result_tmps = function.add_fresh_locals(&result_tys);

                        let mut instrs = save_stack_to_locals(&result_tmps);
                        instrs.extend_from_slice(&[
                            location.0,
                            location.1,
                        ]);
                        instrs.append(&mut restore_locals_with_i64_handling(&result_tmps, &result_tys));
                        instrs.extend_from_slice(&[
                            polymorphic_hooks.get_call(&instr, result_tys),
                            instr,
                        ]);
                        instrs
                    }
                    (_, Call(target_func_idx)) => {
                        let arg_tys = static_info.functions[target_func_idx.0].type_.params.as_slice();
                        let result_tys = static_info.functions[target_func_idx.0].type_.results.as_slice();

                        let arg_tmps = function.add_fresh_locals(arg_tys);
                        let result_tmps = function.add_fresh_locals(result_tys);

                        /* pre call hook */

                        let mut instrs = save_stack_to_locals(&arg_tmps);
                        instrs.extend_from_slice(&[
                            location.0.clone(),
                            location.1.clone(),
                            I32Const(target_func_idx.0 as i32),
                        ]);
                        instrs.append(&mut restore_locals_with_i64_handling(&arg_tmps, &arg_tys));
                        instrs.extend_from_slice(&[
                            polymorphic_hooks.get_call(&instr, arg_tys.to_vec()),
                            instr,
                        ]);

                        /* post call hook */

                        instrs.append(&mut save_stack_to_locals(&result_tmps));
                        instrs.extend_from_slice(&[
                            location.0,
                            location.1,
                        ]);
                        instrs.append(&mut restore_locals_with_i64_handling(&result_tmps, &result_tys));
                        instrs.push(Call(*call_result_hooks.get(result_tys).expect("no call_result hook for tys")));

                        instrs
                    }
                    (_, CallIndirect(func_ty, _ /* table idx == 0 in WASM version 1 */)) => {
                        let arg_tys = func_ty.params.as_slice();
                        let result_tys = func_ty.results.as_slice();

                        let target_table_idx_tmp = function.add_fresh_local(I32);
                        let arg_tmps = function.add_fresh_locals(arg_tys);
                        let result_tmps = function.add_fresh_locals(result_tys);


                        /* pre call hook */

                        let mut instrs = vec![SetLocal(target_table_idx_tmp)];
                        instrs.append(&mut save_stack_to_locals(&arg_tmps));
                        instrs.extend_from_slice(&[
                            GetLocal(target_table_idx_tmp),
                            location.0.clone(),
                            location.1.clone(),
                            GetLocal(target_table_idx_tmp),
                        ]);
                        instrs.append(&mut restore_locals_with_i64_handling(&arg_tmps, &arg_tys));
                        instrs.extend_from_slice(&[
                            polymorphic_hooks.get_call(&instr, arg_tys.to_vec()),
                            instr,
                        ]);

                        /* post call hook */

                        instrs.append(&mut save_stack_to_locals(&result_tmps));
                        instrs.extend_from_slice(&[
                            location.0,
                            location.1,
                        ]);
                        instrs.append(&mut restore_locals_with_i64_handling(&result_tmps, &result_tys));
                        instrs.push(Call(*call_result_hooks.get(result_tys).expect("no call_result hook for tys")));

                        instrs
                    }
                    (Const(ty), instr) => {
                        let mut instrs = vec![
                            location.0,
                            location.1,
                        ];
                        instrs.append(&mut convert_i64_instr(instr.clone(), ty));
                        instrs.extend_from_slice(&[
                            monomorphic_hook_call(&instr),
                            instr,
                        ]);
                        instrs
                    }
                    (Unary { input_ty, result_ty }, instr) => {
                        let input_tmp = function.add_fresh_local(input_ty);
                        let result_tmp = function.add_fresh_local(result_ty);

                        let mut instrs = vec![
                            TeeLocal(input_tmp),
                            instr.clone(),
                            TeeLocal(result_tmp),
                            location.0,
                            location.1,
                        ];
                        // restore saved input and result
                        instrs.append(&mut restore_locals_with_i64_handling(&[input_tmp, result_tmp], &[input_ty, result_ty]));
                        instrs.push(monomorphic_hook_call(&instr));
                        instrs
                    }
                    (Binary { first_ty, second_ty, result_ty }, instr) => {
                        let first_tmp = function.add_fresh_local(first_ty);
                        let second_tmp = function.add_fresh_local(second_ty);
                        let result_tmp = function.add_fresh_local(result_ty);

                        let mut instrs = save_stack_to_locals(&[first_tmp, second_tmp]);
                        instrs.extend_from_slice(&[
                            instr.clone(),
                            TeeLocal(result_tmp),
                            location.0,
                            location.1,
                        ]);
                        instrs.append(&mut restore_locals_with_i64_handling(&[first_tmp, second_tmp, result_tmp], &[first_ty, second_ty, result_ty]));
                        instrs.push(monomorphic_hook_call(&instr));
                        instrs
                    }
                    (MemoryLoad(ty, memarg), instr) => {
                        let addr_tmp = function.add_fresh_local(I32);
                        let value_tmp = function.add_fresh_local(ty);

                        let mut instrs = vec![
                            TeeLocal(addr_tmp),
                            instr.clone(),
                            TeeLocal(value_tmp),
                            location.0,
                            location.1,
                            I32Const(memarg.offset as i32),
                            I32Const(memarg.alignment as i32),
                        ];
                        instrs.append(&mut restore_locals_with_i64_handling(&[addr_tmp, value_tmp], &[I32, ty]));
                        instrs.push(monomorphic_hook_call(&instr));
                        instrs
                    }
                    (MemoryStore(ty, memarg), instr) => {
                        // duplicate stack arguments
                        let addr_tmp = function.add_fresh_local(I32);
                        let value_tmp = function.add_fresh_local(ty);

                        let mut instrs = save_stack_to_locals(&[addr_tmp, value_tmp]);
                        instrs.extend_from_slice(&[
                            instr.clone(),
                            location.0,
                            location.1,
                            I32Const(memarg.offset as i32),
                            I32Const(memarg.alignment as i32),
                        ]);
                        instrs.append(&mut restore_locals_with_i64_handling(&[addr_tmp, value_tmp], &[I32, ty]));
                        instrs.push(monomorphic_hook_call(&instr));
                        instrs
                    }
                    (_, Br(target_label)) => vec![
                        location.0,
                        location.1,
                        I32Const(target_label.0 as i32),
                        I32Const(label_to_instr_idx(&begin_stack, target_label) as i32),
                        Call(br_hook),
                        instr
                    ],
                    // FIXME untested, emscripten seems to not output br_if instruction?
                    (_, BrIf(target_label)) => {
                        let condition_tmp = function.add_fresh_local(I32);
                        vec![
                            TeeLocal(condition_tmp),
                            location.0,
                            location.1,
                            I32Const(target_label.0 as i32),
                            I32Const(label_to_instr_idx(&begin_stack, target_label) as i32),
                            GetLocal(condition_tmp),
                            Call(br_if_hook),
                            instr
                        ]
                    }
                    (_, BrTable(target_table, default_target)) => {
                        static_info.br_tables.push(BrTableInfo::new(
                            target_table.into_iter().map(|label| LabelAndLocation::new(label.0)).collect(),
                            LabelAndLocation::new(default_target.0),
                        ));
                        let target_idx_tmp = function.add_fresh_local(I32);
                        vec![
                            TeeLocal(target_idx_tmp),
                            location.0,
                            location.1,
                            I32Const((static_info.br_tables.len() - 1) as i32),
                            GetLocal(target_idx_tmp),
                            Call(br_table_hook),
                            instr]
                    }
                    _ => unreachable!("no hook for instruction {}", instr.to_instr_name()),
                }
            );
        }

        // finally, move instrumented body inside function
        ::std::mem::replace(&mut function.code.as_mut().unwrap().body, instrumented_body);

        assert!(begin_stack.is_empty(), "invalid begin/end nesting in function {}", fidx.0);
    }

    Some(serde_json::to_string(&static_info).unwrap())
}