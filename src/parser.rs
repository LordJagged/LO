use crate::{ir::*, lexer::*, utils::*, wasm::*};
use alloc::{boxed::Box, collections::BTreeMap, format, str, string::String, vec, vec::Vec};
use LoTokenType::*;

const RECEIVER_PARAM_NAME: &str = "self";

pub fn init<'a>(inspect_mode: bool) -> ModuleContext<'a> {
    let mut ctx = ModuleContext::default();
    ctx.inspect_mode = inspect_mode;

    if ctx.inspect_mode {
        stdout_writeln("[");
    }

    return ctx;
}

pub fn parse_file(
    ctx: &mut ModuleContext,
    file_path: &str,
    loc: &LoLocation,
) -> Result<Option<u32>, LoError> {
    let file_path = resolve_path(file_path, &loc.file_name);
    if ctx.included_modules.contains_key(&file_path) {
        return Ok(None);
    }

    let file = fd_open(&file_path).map_err(|err| LoError {
        message: format!("Cannot load file {file_path}: error code {err}"),
        loc: loc.clone(),
    })?;

    let file_contents = &fd_read_all_and_close(file);
    let file_index = parse_file_contents(ctx, file_path, file_contents)?;
    return Ok(Some(file_index));
}

pub fn parse_file_contents(
    ctx: &mut ModuleContext,
    file_path: String,
    file_contents: &[u8],
) -> Result<u32, LoError> {
    let Ok(file_contents) = str::from_utf8(file_contents) else {
        return Err(LoError {
            message: format!("ParseError: contents of `{file_path}` are not valid UTF-8"),
            loc: LoLocation {
                file_name: file_path.into(),
                ..LoLocation::internal()
            },
        });
    };

    let mut tokens = lex_all(&file_path, file_contents)?;

    let file_index = ctx.included_modules.len() as u32;
    if ctx.inspect_mode {
        stdout_writeln(format!(
            "{{ \"type\": \"file\", \
                \"index\": {file_index}, \
                \"path\": \"{file_path}\" }}, "
        ));
    }
    ctx.included_modules.insert(file_path, file_index);

    parse_file_tokens(ctx, &mut tokens)?;

    return Ok(file_index);
}

fn parse_file_tokens(ctx: &mut ModuleContext, tokens: &mut LoTokenStream) -> Result<(), LoError> {
    while tokens.peek().is_some() {
        parse_top_level_expr(ctx, tokens)?;
        tokens.expect(LoTokenType::Delim, ";")?;
    }

    if let Some(unexpected) = tokens.peek() {
        return Err(LoError {
            message: format!("Unexpected token on top level: {}", unexpected.value),
            loc: unexpected.loc.clone(),
        });
    }

    Ok(())
}

pub fn finalize(ctx: &mut ModuleContext) -> Result<(), LoError> {
    if !ctx.inspect_mode {
        // push function exports
        for fn_export in &ctx.fn_exports {
            let fn_def = ctx.fn_defs.get(&fn_export.in_name).unwrap(); // safe

            ctx.wasm_module.borrow_mut().exports.push(WasmExport {
                export_type: WasmExportType::Func,
                export_name: fn_export.out_name.clone(),
                exported_item_index: fn_def.get_absolute_index(ctx),
            });
        }
    }

    // push function codes
    for mut fn_body in ctx.fn_bodies.take() {
        let fn_def = ctx
            .fn_defs
            .values()
            .find(|fd| fd.local && fd.fn_index == fn_body.fn_index)
            .unwrap();

        let mut fn_ctx = FnContext {
            module: &ctx,
            lo_fn_type: &fn_def.type_,
            locals_last_index: fn_body.locals_last_index,
            non_arg_wasm_locals: vec![],
            defers: vec![],
        };

        let locals_block = Block {
            locals: fn_body.locals,
            ..Default::default()
        };

        let mut block_ctx = BlockContext {
            module: &ctx,
            fn_ctx: &mut fn_ctx,
            block: Block {
                parent: Some(&locals_block),
                block_type: BlockType::Function,
                ..Default::default()
            },
        };

        let mut contents = parse_block_contents(&mut block_ctx, &mut fn_body.body, LoType::Void)?;

        if !contents.has_return && !contents.has_never {
            if let Some(mut values) = get_deferred(&mut block_ctx) {
                contents.exprs.append(&mut values);
            };

            let return_type = &fn_def.type_.output;

            match return_type {
                LoType::Void => {}
                LoType::Result { ok_type, err_type } if *ok_type.as_ref() == LoType::Void => {
                    contents.exprs.push(err_type.get_default_value(ctx));
                }
                LoType::Never => {
                    return Err(LoError {
                        message: format!("This function terminates but is marked as `never`"),
                        // TODO: this should point to function definition instead
                        loc: fn_body.body.terminal_token.loc,
                    });
                }
                _ => {
                    return Err(LoError {
                        message: format!("Missing return expression"),
                        // TODO: this should point to function definition instead
                        loc: fn_body.body.terminal_token.loc,
                    });
                }
            }
        }

        let mut locals = Vec::<WasmLocals>::new();
        for local_type in &block_ctx.fn_ctx.non_arg_wasm_locals {
            if let Some(wasm_locals) = locals.last_mut() {
                if (*wasm_locals).value_type == *local_type {
                    wasm_locals.count += 1;
                    continue;
                }
            }
            locals.push(WasmLocals {
                count: 1,
                value_type: local_type.clone(),
            });
        }

        let mut instrs = vec![];
        lower_exprs(&mut instrs, contents.exprs);

        ctx.wasm_module.borrow_mut().codes.push(WasmFn {
            locals,
            expr: WasmExpr { instrs },
        });
    }

    if !ctx.inspect_mode {
        // lower global values (a hack to resolve __DATA_SIZE__ after all strings were seen)
        for GlobalDef { index, value, .. } in ctx.globals.values() {
            lower_expr(
                &mut ctx.wasm_module.borrow_mut().globals[*index as usize]
                    .initial_value
                    .instrs,
                value.clone(),
            );
        }
    }

    if !ctx.inspect_mode {
        write_debug_info(ctx)?;
    }

    if ctx.inspect_mode {
        stdout_writeln("{ \"type\": \"end\" }");

        stdout_writeln("]");
    }

    Ok(())
}

// TODO: consider adding module name if needed
// TODO: add local names (requires sizable refactoring to achieve)
fn write_debug_info(ctx: &mut ModuleContext) -> Result<(), LoError> {
    use crate::wasm::*;

    let mut wasm_module = ctx.wasm_module.borrow_mut();

    let section_name = "name";
    write_u32(&mut wasm_module.custom, section_name.len() as u32);
    write_all(&mut wasm_module.custom, section_name.as_bytes());

    let mut subsection_buf = Vec::new();

    let first_own_fn_index = ctx.imported_fns_count;
    let own_fns_count = wasm_module.functions.len() as u32;

    /* function names */
    {
        write_u32(&mut subsection_buf, own_fns_count);

        for fn_index in first_own_fn_index..first_own_fn_index + own_fns_count {
            // TODO: this is really bad
            let (fn_name, _) = ctx
                .fn_defs
                .iter()
                .find(|(_, v)| v.get_absolute_index(ctx) == fn_index)
                .unwrap();

            write_u32(&mut subsection_buf, fn_index);
            write_u32(&mut subsection_buf, fn_name.len() as u32);
            write_all(&mut subsection_buf, fn_name.as_bytes());
        }

        write_section(&mut wasm_module.custom, &mut subsection_buf, 1);
    }

    Ok(())
}

fn parse_top_level_expr(
    ctx: &mut ModuleContext,
    tokens: &mut LoTokenStream,
) -> Result<(), LoError> {
    if tokens.peek().is_none() {
        return Ok(());
    }

    if let Some(_) = tokens.eat(Symbol, "fn")? {
        return parse_fn_def(ctx, tokens, false);
    }

    if let Some(_) = tokens.eat(Symbol, "macro")? {
        return parse_macro_def(ctx, tokens);
    }

    if let Some(_) = tokens.eat(Symbol, "memory")? {
        return parse_memory(ctx, tokens, false);
    }

    if let Some(_) = tokens.eat(Symbol, "export")? {
        if let Some(_) = tokens.eat(Symbol, "fn")? {
            return parse_fn_def(ctx, tokens, true);
        }

        if let Some(_) = tokens.eat(Symbol, "memory")? {
            return parse_memory(ctx, tokens, true);
        }

        if let Some(_) = tokens.eat(Symbol, "existing")? {
            tokens.expect(Symbol, "fn")?;
            let in_name = parse_nested_symbol(tokens)?;
            if let None = ctx.fn_defs.get(&in_name.value) {
                return Err(LoError {
                    message: format!("Cannot export unknown function {}", in_name.value),
                    loc: in_name.loc,
                });
            }

            tokens.expect(Symbol, "as")?;
            let out_name = tokens.expect_any(StringLiteral)?.clone();

            ctx.fn_exports.push(FnExport {
                in_name: in_name.value,
                out_name: out_name.value,
            });

            return Ok(());
        }
    }

    if let Some(_) = tokens.eat(Symbol, "import")? {
        tokens.expect(Symbol, "from")?;
        let module_name = tokens.expect_any(StringLiteral)?.clone();

        tokens.expect(Delim, "{")?;
        while let None = tokens.eat(Delim, "}")? {
            tokens.expect(Symbol, "fn")?;
            let fn_decl = parse_fn_decl(ctx, tokens)?;
            tokens.expect(LoTokenType::Delim, ";")?;

            if ctx.fn_defs.contains_key(&fn_decl.fn_name) {
                return Err(LoError {
                    message: format!("Cannot redefine function: {}", fn_decl.fn_name),
                    loc: fn_decl.loc,
                });
            }

            let type_index = ctx.insert_fn_type(fn_decl.wasm_type);

            let fn_index = ctx.imported_fns_count;
            ctx.imported_fns_count += 1;

            let fn_def = FnDef {
                local: false,
                fn_index,
                fn_params: fn_decl.fn_params,
                type_index,
                type_: fn_decl.lo_type,
            };
            ctx.fn_defs.insert(fn_decl.fn_name.clone(), fn_def);
            ctx.wasm_module.borrow_mut().imports.push(WasmImport {
                module_name: module_name.value.clone(),
                item_name: fn_decl.method_name,
                item_desc: WasmImportDesc::Func { type_index },
            });
        }

        return Ok(());
    }

    if let Some(_) = tokens.eat(Symbol, "let")?.cloned() {
        let mutable = true;
        let global_name = parse_nested_symbol(tokens)?;
        tokens.expect(Operator, "=")?;

        let global_value_loc = tokens.loc().clone();
        let global_value = parse_const_expr(ctx, tokens, 0)?;

        let lo_type = global_value.get_type(ctx);
        let Some(wasm_type) = lo_type.to_wasm_type() else {
            return Err(LoError {
                message: format!("Unsupported type: {lo_type}"),
                loc: global_value_loc,
            });
        };

        if ctx.globals.contains_key(&global_name.value) {
            return Err(LoError {
                message: format!("Cannot redefine global: {}", global_name.value),
                loc: global_name.loc,
            });
        }

        if ctx.inspect_mode {
            let source_index = ctx
                .included_modules
                .get(&global_name.loc.file_name as &str)
                .unwrap();

            let sl = global_name.loc.pos.line;
            let sc = global_name.loc.pos.col;
            let el = global_name.loc.end_pos.line;
            let ec = global_name.loc.end_pos.col;

            let global_name = &global_name.value;

            stdout_writeln(format!(
                "{{ \"type\": \"hover\", \
                   \"source\": {source_index}, \
                   \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                   \"content\": \"let {global_name}: {lo_type}\" }}, "
            ));
        }

        ctx.globals.insert(
            global_name.value.clone(),
            GlobalDef {
                index: ctx.globals.len() as u32,
                mutable,
                value_type: lo_type,
                value: global_value,
            },
        );

        ctx.wasm_module.borrow_mut().globals.push(WasmGlobal {
            kind: WasmGlobalKind {
                value_type: wasm_type,
                mutable,
            },
            initial_value: WasmExpr { instrs: vec![] }, // will be filled in `finalize`
        });

        return Ok(());
    }

    if let Some(_) = tokens.eat(Symbol, "struct")? {
        let struct_name = parse_nested_symbol(tokens)?;

        if let Some(_) = ctx.type_scope.get(&struct_name.value) {
            return Err(LoError {
                message: format!("Cannot redefine type {}", struct_name.value),
                loc: struct_name.loc,
            });
        }

        // declare not fully defined struct to use in self-references
        ctx.struct_defs.insert(
            struct_name.value.clone(),
            StructDef {
                fields: vec![],
                fully_defined: false,
            },
        );

        ctx.type_scope.insert(
            struct_name.value.clone(),
            LoType::StructInstance {
                name: struct_name.value.clone(),
            },
        );

        let mut field_index = 0;
        let mut byte_offset = 0;
        let mut struct_fields = Vec::<StructField>::new();

        tokens.expect(Delim, "{")?;
        while let None = tokens.eat(Delim, "}")? {
            let field_name = tokens.expect_any(Symbol)?.clone();
            tokens.expect(Operator, ":")?;
            let field_type_loc = tokens.loc().clone();
            let field_type = parse_const_lo_type(ctx, tokens)?;
            if !tokens.next_is(Delim, "}")? {
                tokens.expect(Delim, ",")?;
            }

            if struct_fields
                .iter()
                .find(|f| f.name == field_name.value)
                .is_some()
            {
                return Err(LoError {
                    message: format!(
                        "Found duplicate struct field name: '{}' of struct {}",
                        field_name.value, struct_name.value,
                    ),
                    loc: field_name.loc,
                });
            }

            let mut stats = EmitComponentStats::default();
            field_type
                .emit_sized_component_stats(ctx, &mut stats, &mut vec![])
                .map_err(|err| LoError {
                    message: err,
                    loc: field_type_loc,
                })?;

            struct_fields.push(StructField {
                name: field_name.value,
                value_type: field_type,
                field_index,
                byte_offset,
            });

            field_index += stats.count;
            byte_offset += stats.byte_length;
        }

        let struct_def = ctx.struct_defs.get_mut(&struct_name.value).unwrap();
        struct_def.fields.append(&mut struct_fields);
        struct_def.fully_defined = true;

        return Ok(());
    }

    if let Some(_) = tokens.eat(Symbol, "type")?.cloned() {
        let type_alias = parse_nested_symbol(tokens)?;
        tokens.expect(Operator, "=")?;
        let actual_type = parse_const_lo_type(ctx, tokens)?;

        if let Some(_) = ctx.type_scope.get(&type_alias.value) {
            return Err(LoError {
                message: format!("Cannot redefine type: {}", type_alias.value),
                loc: type_alias.loc.clone(),
            });
        }

        ctx.type_scope.insert(type_alias.value, actual_type);

        return Ok(());
    }

    if let Some(_) = tokens.eat(Symbol, "const")?.cloned() {
        let const_name = parse_nested_symbol(tokens)?;
        tokens.expect(Operator, "=")?;
        let const_value = parse_const_expr(ctx, tokens, 0)?;

        if ctx.constants.borrow().contains_key(&const_name.value) {
            return Err(LoError {
                message: format!("Duplicate constant: {}", const_name.value),
                loc: const_name.loc.clone(),
            });
        }

        if ctx.inspect_mode {
            let source_index = ctx
                .included_modules
                .get(&const_name.loc.file_name as &str)
                .unwrap();

            let sl = const_name.loc.pos.line;
            let sc = const_name.loc.pos.col;
            let el = const_name.loc.end_pos.line;
            let ec = const_name.loc.end_pos.col;

            let const_name = &const_name.value;
            let const_type = const_value.get_type(ctx);

            stdout_writeln(format!(
                "{{ \"type\": \"hover\", \
                   \"source\": {source_index}, \
                   \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                   \"content\": \"const {const_name}: {const_type}\" }}, "
            ));
        }

        ctx.constants
            .borrow_mut()
            .insert(const_name.value, const_value);

        return Ok(());
    }

    if let Some(_) = tokens.eat(Symbol, "include")?.cloned() {
        let file_path = tokens.expect_any(StringLiteral)?;
        let target_index = parse_file(ctx, &file_path.value, &file_path.loc)?;
        let Some(target_index) = target_index else {
            return Ok(());
        };

        if ctx.inspect_mode {
            let source_index = ctx
                .included_modules
                .get(&file_path.loc.file_name as &str)
                .unwrap();

            let sl = file_path.loc.pos.line;
            let sc = file_path.loc.pos.col;
            let el = file_path.loc.end_pos.line;
            let ec = file_path.loc.end_pos.col;

            stdout_writeln(format!(
                "{{ \"type\": \"link\", \
                    \"source\": {source_index}, \
                    \"sourceRange\": \"{sl}:{sc}-{el}:{ec}\", \
                    \"target\": {target_index}, \
                    \"targetRange\": \"1:1-1:1\" }}, ",
            ));
        }

        return Ok(());
    }

    let unexpected = tokens.peek().unwrap();
    return Err(LoError {
        message: format!("Unexpected top level token: {}", unexpected.value),
        loc: unexpected.loc.clone(),
    });
}

fn parse_memory(
    ctx: &mut ModuleContext,
    tokens: &mut LoTokenStream,
    exported: bool,
) -> Result<(), LoError> {
    if let Some(_) = tokens.eat(Operator, "@")? {
        let offset = parse_u32_literal(tokens.expect_any(IntLiteral)?)?;
        tokens.expect(Operator, "=")?;
        let data = tokens.expect_any(StringLiteral)?;

        let bytes = data.value.as_bytes().iter().map(|b| *b).collect();

        ctx.wasm_module.borrow_mut().datas.push(WasmData::Active {
            offset: WasmExpr {
                instrs: vec![WasmInstr::I32Const {
                    value: offset as i32,
                }],
            },
            bytes,
        });

        return Ok(());
    }

    let memory_name = String::from("memory");
    if ctx.memories.contains_key(&memory_name) {
        return Err(LoError {
            message: format!("Duplicate memory definition: {memory_name}"),
            loc: tokens.peek().unwrap().loc.clone(),
        });
    }

    let mut memory_limits = WasmLimits { min: 0, max: None };

    tokens.expect(Delim, "{")?;
    while let None = tokens.eat(Delim, "}")? {
        let prop = tokens.expect_any(Symbol)?.clone();
        match prop.value.as_str() {
            "min_pages" => {
                tokens.expect(Operator, ":")?;
                let value = parse_u32_literal(tokens.expect_any(IntLiteral)?)?;
                memory_limits.min = value;
            }
            "max_pages" => {
                tokens.expect(Operator, ":")?;
                let value = parse_u32_literal(tokens.expect_any(IntLiteral)?)?;
                memory_limits.max = Some(value);
            }
            _ => {
                return Err(LoError {
                    message: format!("ayo"),
                    loc: prop.loc,
                })
            }
        }
    }

    let memory_index = ctx.wasm_module.borrow().memories.len() as u32;
    ctx.wasm_module.borrow_mut().memories.push(memory_limits);
    ctx.memories.insert(memory_name.clone(), memory_index);

    if exported {
        ctx.wasm_module.borrow_mut().exports.push(WasmExport {
            export_type: WasmExportType::Mem,
            export_name: "memory".into(),
            exported_item_index: memory_index,
        });
    }

    Ok(())
}

fn parse_fn_def(
    ctx: &mut ModuleContext,
    tokens: &mut LoTokenStream,
    exported: bool,
) -> Result<(), LoError> {
    let fn_decl = parse_fn_decl(ctx, tokens)?;
    let body = collect_block_tokens(tokens)?;

    if ctx.fn_defs.contains_key(&fn_decl.fn_name) {
        return Err(LoError {
            message: format!("Cannot redefine function: {}", fn_decl.fn_name),
            loc: fn_decl.loc,
        });
    }

    if exported {
        ctx.fn_exports.push(FnExport {
            in_name: fn_decl.fn_name.clone(),
            out_name: fn_decl.fn_name.clone(),
        });
    }

    let locals_last_index = fn_decl.wasm_type.inputs.len() as u32;
    let type_index = ctx.insert_fn_type(fn_decl.wasm_type);
    ctx.wasm_module.borrow_mut().functions.push(type_index);

    let fn_index = ctx.wasm_module.borrow_mut().functions.len() as u32 - 1;

    ctx.fn_defs.insert(
        fn_decl.fn_name,
        FnDef {
            local: true,
            fn_index,
            fn_params: fn_decl.fn_params,
            type_index,
            type_: fn_decl.lo_type,
        },
    );

    ctx.fn_bodies.borrow_mut().push(FnBody {
        fn_index,
        type_index,
        locals: fn_decl.locals,
        locals_last_index,
        body,
    });

    return Ok(());
}

fn parse_macro_def(ctx: &mut ModuleContext, tokens: &mut LoTokenStream) -> Result<(), LoError> {
    let macro_name = parse_nested_symbol(tokens)?;
    tokens.expect(Operator, "!")?;

    if ctx.macros.contains_key(&macro_name.value) {
        return Err(LoError {
            message: format!("Cannot redefine macro: {}", macro_name.value),
            loc: macro_name.loc,
        });
    }

    let (receiver_type, method_name) = extract_method_receiver_and_name(ctx, &macro_name)?;
    let mut type_params = Vec::<String>::new();

    if let Some(_) = tokens.eat(Operator, "<")? {
        while let None = tokens.eat(Operator, ">")? {
            let p_name = tokens.expect_any(Symbol)?.clone();
            if !tokens.next_is(Operator, ">")? {
                tokens.expect(Delim, ",")?;
            }

            if get_type_by_name(ctx, &ctx.type_scope, &p_name, false).is_ok() {
                return Err(LoError {
                    message: format!("Type parameter shadows existing type: {}", p_name.value),
                    loc: p_name.loc.clone(),
                });
            }

            for param in &type_params {
                if *param == p_name.value {
                    return Err(LoError {
                        message: format!("Found duplicate type parameter: {}", p_name.value),
                        loc: p_name.loc.clone(),
                    });
                }
            }

            type_params.push(p_name.value);
        }
    }

    let mut new_type_scope = LoTypeScope {
        parent: Some(&ctx.type_scope),
        ..Default::default()
    };
    for type_param in &type_params {
        new_type_scope.insert(
            type_param.clone(),
            LoType::MacroTypeArg {
                name: type_param.clone(),
            },
        )
    }

    let params = parse_fn_params(ctx, &new_type_scope, tokens, &receiver_type)?;
    let return_type = if let Some(_) = tokens.eat(Operator, ":")? {
        parse_lo_type_(ctx, &new_type_scope, tokens, false)?
    } else {
        LoType::Void
    };
    let body = collect_block_tokens(tokens)?;

    ctx.macros.insert(
        macro_name.value.clone(),
        MacroDef {
            receiver_type,
            method_name,
            type_params,
            params,
            return_type,
            body,
        },
    );

    return Ok(());
}

struct FnDecl {
    fn_name: String,
    method_name: String,
    loc: LoLocation,
    fn_params: Vec<FnParam>,
    lo_type: LoFnType,
    wasm_type: WasmFnType,
    locals: BTreeMap<String, LocalDef>,
}

fn parse_fn_decl(ctx: &mut ModuleContext, tokens: &mut LoTokenStream) -> Result<FnDecl, LoError> {
    let fn_name = parse_nested_symbol(tokens)?;
    let (receiver_type, method_name) = extract_method_receiver_and_name(ctx, &fn_name)?;

    let params = parse_fn_params(ctx, &ctx.type_scope, tokens, &receiver_type)?;

    let mut fn_decl = FnDecl {
        fn_name: fn_name.value.clone(),
        fn_params: params.clone(),
        method_name,
        loc: fn_name.loc.clone(),
        lo_type: LoFnType {
            inputs: vec![],
            output: LoType::Void,
        },
        wasm_type: WasmFnType {
            inputs: vec![],
            outputs: vec![],
        },
        locals: BTreeMap::new(),
    };

    for param in params {
        let local_def = LocalDef {
            index: fn_decl.wasm_type.inputs.len() as u32,
            value_type: param.type_.clone(),
        };
        fn_decl.locals.insert(param.name, local_def);

        param
            .type_
            .emit_components(ctx, &mut fn_decl.wasm_type.inputs);

        fn_decl.lo_type.inputs.push(param.type_);
    }

    let lo_output = if let Some(_) = tokens.eat(Operator, ":")? {
        parse_const_lo_type(ctx, tokens)?
    } else {
        LoType::Void
    };

    lo_output.emit_components(&ctx, &mut fn_decl.wasm_type.outputs);
    fn_decl.lo_type.output = lo_output;

    Ok(fn_decl)
}

fn parse_fn_params(
    ctx: &ModuleContext,
    type_scope: &LoTypeScope,
    tokens: &mut LoTokenStream,
    receiver_type: &Option<LoType>,
) -> Result<Vec<FnParam>, LoError> {
    let mut params = Vec::new();

    tokens.expect(Delim, "(")?;

    if let Some(receiver_type) = &receiver_type {
        if let Some(_) = tokens.eat(Symbol, RECEIVER_PARAM_NAME)? {
            if !tokens.next_is(Delim, ")")? {
                tokens.expect(Delim, ",")?;
            }

            params.push(FnParam {
                name: String::from(RECEIVER_PARAM_NAME),
                type_: receiver_type.clone(),
            });
        } else if let Some(_) = tokens.eat(Operator, "&")? {
            tokens.expect(Symbol, RECEIVER_PARAM_NAME)?;
            if !tokens.next_is(Delim, ")")? {
                tokens.expect(Delim, ",")?;
            }

            params.push(FnParam {
                name: String::from(RECEIVER_PARAM_NAME),
                type_: LoType::Pointer(Box::new(receiver_type.clone())),
            });
        };
    }

    while let None = tokens.eat(Delim, ")")? {
        let p_name = tokens.expect_any(Symbol)?.clone();
        tokens.expect(Operator, ":")?;
        let p_type = parse_lo_type_(ctx, type_scope, tokens, false)?;
        if !tokens.next_is(Delim, ")")? {
            tokens.expect(Delim, ",")?;
        }

        for param in &params {
            if param.name == p_name.value {
                return Err(LoError {
                    message: format!(
                        "Found function param with conflicting name: {}",
                        p_name.value
                    ),
                    loc: p_name.loc.clone(),
                });
            }
        }

        params.push(FnParam {
            name: p_name.value,
            type_: p_type,
        });
    }

    Ok(params)
}

fn parse_block(
    ctx: &mut BlockContext,
    tokens: &mut LoTokenStream,
) -> Result<Vec<LoInstr>, LoError> {
    let mut block_tokens = collect_block_tokens(tokens)?;
    let contents = parse_block_contents(ctx, &mut block_tokens, LoType::Void)?;
    Ok(contents.exprs)
}

fn collect_block_tokens(tokens: &mut LoTokenStream) -> Result<LoTokenStream, LoError> {
    let mut output = LoTokenStream::new(vec![], LoLocation::internal());

    let mut depth = 0;
    tokens.expect(Delim, "{")?;
    loop {
        if let Some(t) = tokens.eat(Delim, "{")? {
            output.tokens.push(t.clone());
            depth += 1;
            continue;
        }
        if let Some(t) = tokens.eat(Delim, "}")? {
            if depth == 0 {
                output.terminal_token = t.clone();
                break;
            }
            output.tokens.push(t.clone());
            depth -= 1;
            continue;
        }
        output.tokens.push(tokens.next().unwrap().clone());
    }

    Ok(output)
}

fn parse_expr(
    ctx: &mut BlockContext,
    tokens: &mut LoTokenStream,
    min_bp: u32,
) -> Result<LoInstr, LoError> {
    let mut primary = parse_primary(ctx, tokens)?;

    while tokens.peek().is_some() {
        let op_symbol = tokens.peek().unwrap().clone();
        let Some(op) = InfixOp::parse(op_symbol) else {
            break;
        };

        if op.info.bp < min_bp {
            break;
        }

        tokens.next(); // skip operator
        primary = parse_postfix(ctx, tokens, primary, op)?;
    }

    Ok(primary)
}

fn parse_primary(ctx: &mut BlockContext, tokens: &mut LoTokenStream) -> Result<LoInstr, LoError> {
    if tokens.next_is_any(IntLiteral)? {
        return parse_const_int(tokens);
    }

    if let Some(value) = tokens.eat_any(CharLiteral)? {
        return Ok(LoInstr::U32Const {
            value: value.value.chars().next().unwrap() as u32,
        });
    }

    if let Some(value) = tokens.eat_any(StringLiteral)? {
        return Ok(build_const_str_instr(ctx.module, &value.value));
    }

    if let Some(_) = tokens.eat(Symbol, "true")?.cloned() {
        return Ok(LoInstr::U32Const { value: 1 }.casted(LoType::Bool));
    }

    if let Some(_) = tokens.eat(Symbol, "false")?.cloned() {
        return Ok(LoInstr::U32Const { value: 0 }.casted(LoType::Bool));
    }

    if let Some(_) = tokens.eat(Symbol, "__DATA_SIZE__")? {
        return Ok(LoInstr::U32ConstLazy {
            value: ctx.module.data_size.clone(),
        });
    }

    if let Some(_) = tokens.eat(Symbol, "unreachable")? {
        return Ok(LoInstr::Unreachable);
    }

    if let Some(_) = tokens.eat(Delim, "(")? {
        let expr = parse_expr(ctx, tokens, 0)?;
        tokens.expect(Delim, ")")?;
        return Ok(expr);
    }

    if let Some(return_token) = tokens.eat(Symbol, "return")?.cloned() {
        let value = if tokens.peek().is_none() || tokens.next_is(Delim, ";")? {
            LoInstr::NoInstr
        } else {
            parse_expr(ctx, tokens, 0)?
        };

        let return_type = value.get_type(ctx.module);
        let (expected_return_type, error_type) =
            if let LoType::Result { ok_type, err_type } = &ctx.fn_ctx.lo_fn_type.output {
                (ok_type.as_ref(), Some(err_type))
            } else {
                (&ctx.fn_ctx.lo_fn_type.output, None)
            };

        if return_type != *expected_return_type {
            return Err(LoError {
                message: format!(
                    "TypeError: Invalid return type, \
                        expected {expected_return_type}, got {return_type}",
                ),
                loc: return_token.loc,
            });
        }

        let mut return_value = if let Some(error_type) = error_type {
            LoInstr::MultiValueEmit {
                values: vec![value, error_type.get_default_value(ctx.module)],
            }
        } else {
            value
        };

        if let Some(mut values) = get_deferred(ctx) {
            values.insert(0, return_value);
            return_value = LoInstr::MultiValueEmit { values }.casted(LoType::Void);
        }

        return Ok(LoInstr::Return {
            value: Box::new(return_value),
        });
    }

    if let Some(throw_token) = tokens.eat(Symbol, "throw")?.cloned() {
        let error = if tokens.peek().is_none() || tokens.next_is(Delim, ";")? {
            LoInstr::NoInstr
        } else {
            parse_expr(ctx, tokens, 0)?
        };

        let error_type = error.get_type(ctx.module);
        let LoType::Result { ok_type, err_type } = &ctx.fn_ctx.lo_fn_type.output else {
            return Err(LoError {
                message: format!(
                    "TypeError: Cannot throw {error_type}, function can only return {output}",
                    output = ctx.fn_ctx.lo_fn_type.output,
                ),
                loc: throw_token.loc,
            });
        };
        if error_type != **err_type {
            return Err(LoError {
                message: format!(
                    "TypeError: Invalid throw type, expected {err_type}, got {error_type}",
                ),
                loc: throw_token.loc,
            });
        }

        let mut return_value = LoInstr::MultiValueEmit {
            values: vec![ok_type.get_default_value(ctx.module), error],
        };

        if let Some(mut values) = get_deferred(ctx) {
            values.insert(0, return_value);
            return_value = LoInstr::MultiValueEmit { values }.casted(LoType::Void);
        }

        return Ok(LoInstr::Return {
            value: Box::new(return_value),
        });
    }

    if let Some(t) = tokens.eat(Symbol, "sizeof")?.cloned() {
        let value_type = parse_lo_type(ctx, tokens)?;

        return Ok(LoInstr::U32Const {
            value: value_type
                .sized_comp_stats(&ctx.module)
                .map_err(|err| LoError {
                    message: err,
                    loc: t.loc.clone(),
                })?
                .byte_length as u32,
        });
    }

    if let Some(_) = tokens.eat(Symbol, "defer")? {
        let deffered_expr = parse_expr(ctx, tokens, 0)?;

        ctx.fn_ctx.defers.push(deffered_expr);

        return Ok(LoInstr::NoInstr);
    }

    if let Some(_) = tokens.eat(Symbol, "__memory_size")? {
        tokens.expect(Delim, "(")?;
        tokens.expect(Delim, ")")?;
        return Ok(LoInstr::MemorySize {});
    }

    if let Some(t) = tokens.eat(Symbol, "__memory_grow")?.cloned() {
        tokens.expect(Delim, "(")?;
        let size = parse_expr(ctx, tokens, 0)?;
        tokens.expect(Delim, ")")?;

        let size_type = size.get_type(ctx.module);
        if size_type != LoType::U32 {
            return Err(LoError {
                message: format!("Invalid arguments for {}", t.value),
                loc: t.loc,
            });
        };

        return Ok(LoInstr::MemoryGrow {
            size: Box::new(size),
        });
    }

    if let Some(t) = tokens.eat(Symbol, "__debug_typeof")?.cloned() {
        let loc = tokens.peek().unwrap_or(&t).loc.clone();

        let expr = parse_expr(ctx, tokens, 0)?;
        let expr_type = expr.get_type(ctx.module);
        crate::utils::debug(format!(
            "{}",
            LoError {
                message: format!("{expr_type:?}"),
                loc,
            }
        ));
        return Ok(LoInstr::NoInstr);
    }

    if let Some(dbg_token) = tokens.eat(Symbol, "dbg")?.cloned() {
        let message = tokens.expect_any(StringLiteral)?;
        let debug_mesage = format!("{} - {}", dbg_token.loc, message.value);
        return Ok(build_const_str_instr(ctx.module, &debug_mesage));
    }

    if let Some(_) = tokens.eat(Symbol, "if")? {
        let cond = parse_expr(ctx, tokens, 0)?;

        let then_branch = parse_block(
            &mut BlockContext {
                module: ctx.module,
                fn_ctx: ctx.fn_ctx,
                block: Block {
                    parent: Some(&ctx.block),
                    ..Default::default()
                },
            },
            tokens,
        )?;

        let else_branch = if let Some(_) = tokens.eat(Symbol, "else")? {
            Some(parse_block(
                &mut BlockContext {
                    module: ctx.module,
                    fn_ctx: ctx.fn_ctx,
                    block: Block {
                        parent: Some(&ctx.block),
                        ..Default::default()
                    },
                },
                tokens,
            )?)
        } else {
            None
        };

        return Ok(LoInstr::If {
            block_type: LoType::Void,
            cond: Box::new(cond),
            then_branch,
            else_branch,
        });
    }

    if let Some(_) = tokens.eat(Symbol, "loop")? {
        let mut ctx = BlockContext {
            module: ctx.module,
            fn_ctx: ctx.fn_ctx,
            block: Block {
                parent: Some(&ctx.block),
                block_type: BlockType::Loop,
                ..Default::default()
            },
        };

        let mut body = parse_block(&mut ctx, tokens)?;

        let implicit_continue = LoInstr::Branch { label_index: 0 };
        body.push(implicit_continue);

        return Ok(LoInstr::Block {
            block_type: LoType::Void,
            body: vec![LoInstr::Loop {
                block_type: LoType::Void,
                body,
            }],
        });
    }

    if let Some(for_loop) = tokens.eat(Symbol, "for")?.cloned() {
        let counter = tokens.expect_any(Symbol).cloned()?;
        tokens.expect(Symbol, "in")?;
        let counter_ctx = &mut BlockContext {
            module: ctx.module,
            fn_ctx: ctx.fn_ctx,
            block: Block {
                parent: Some(&ctx.block),
                block_type: BlockType::Block,
                ..Default::default()
            },
        };

        let start_count = parse_expr(counter_ctx, tokens, 0)?;
        tokens.expect(Operator, "..")?;
        let end_count = parse_expr(counter_ctx, tokens, 0)?;

        let counter_type = start_count.get_type(counter_ctx.module);
        if end_count.get_type(counter_ctx.module) != counter_type {
            return Err(LoError {
                message: format!(
                    "Invalid end count type: {}, expected: {counter_type}",
                    end_count.get_type(counter_ctx.module)
                ),
                loc: for_loop.loc,
            });
        }

        let check_op_kind;
        let add_op_kind;
        let step_instr;
        match counter_type {
            LoType::Bool | LoType::I8 | LoType::U8 | LoType::I32 | LoType::U32 => {
                check_op_kind = WasmBinaryOpKind::I32_EQ;
                add_op_kind = WasmBinaryOpKind::I32_ADD;
                step_instr = LoInstr::U32Const { value: 1 };
            }
            LoType::I64 | LoType::U64 => {
                check_op_kind = WasmBinaryOpKind::I64_EQ;
                add_op_kind = WasmBinaryOpKind::I64_ADD;
                step_instr = LoInstr::U64Const { value: 1 };
            }
            _ => {
                return Err(LoError {
                    message: format!("Invalid counter type: {counter_type}",),
                    loc: for_loop.loc,
                })
            }
        };

        let init_instr = define_local(counter_ctx, &counter, start_count, counter_type.clone())?;
        let get_counter_instr = LoInstr::LocalGet {
            local_index: counter_ctx
                .block
                .get_own_local(&counter.value)
                .unwrap()
                .index,
            value_type: counter_type.clone(),
        };

        let break_instr = LoInstr::Branch { label_index: 2 };
        let implicit_continue = LoInstr::Branch { label_index: 0 };

        let end_check_instr = LoInstr::If {
            block_type: LoType::Void,
            cond: Box::new(LoInstr::BinaryOp {
                kind: check_op_kind,
                lhs: Box::new(get_counter_instr.clone()),
                rhs: Box::new(end_count),
            }),
            then_branch: vec![break_instr],
            else_branch: None,
        };
        let update_instr = compile_set(
            counter_ctx,
            LoInstr::BinaryOp {
                kind: add_op_kind,
                lhs: Box::new(get_counter_instr.clone()),
                rhs: Box::new(step_instr),
            },
            get_counter_instr,
            &for_loop.loc,
        )?;

        let loop_body_ctx = &mut BlockContext {
            module: counter_ctx.module,
            fn_ctx: counter_ctx.fn_ctx,
            block: Block {
                parent: Some(&counter_ctx.block),
                block_type: BlockType::ForLoop,
                ..Default::default()
            },
        };
        let loop_body = parse_block(loop_body_ctx, tokens)?;

        let instrs = vec![
            init_instr,
            LoInstr::Block {
                block_type: LoType::Void,
                body: vec![LoInstr::Loop {
                    body: vec![
                        end_check_instr,
                        LoInstr::Block {
                            block_type: LoType::Void,
                            body: loop_body,
                        },
                        update_instr,
                        implicit_continue,
                    ],
                    block_type: LoType::Void,
                }],
            },
        ];

        return Ok(LoInstr::MultiValueEmit { values: instrs }.casted(LoType::Void));

        // let mut ctx = BlockContext {
        //     module: ctx.module,
        //     fn_ctx: ctx.fn_ctx,
        //     block: Block {
        //         parent: Some(&ctx.block),
        //         block_type: BlockType::Loop,
        //         ..Default::default()
        //     },
        // };

        // let mut body = parse_block(&mut ctx, tokens)?;

        // let implicit_continue = LoInstr::Branch { label_index: 0 };
        // body.push(implicit_continue);

        // return Ok(LoInstr::Block {
        //     block_type: LoType::Void,
        //     body: vec![LoInstr::Loop {
        //         block_type: LoType::Void,
        //         body,
        //     }],
        // });
    }

    if let Some(_) = tokens.eat(Symbol, "break")? {
        let mut label_index = 1; // 0 = loop, 1 = loop wrapper block

        let mut current_block = &ctx.block;
        loop {
            if current_block.block_type == BlockType::Loop {
                break;
            }

            if current_block.block_type == BlockType::ForLoop {
                label_index += 1;
                break;
            }

            current_block = current_block.parent.unwrap();
            label_index += 1;
        }

        return Ok(LoInstr::Branch { label_index });
    }

    if let Some(_) = tokens.eat(Symbol, "continue")? {
        let mut label_index = 0; // 0 = loop, 1 = loop wrapper block

        let mut current_block = &ctx.block;
        loop {
            if current_block.block_type == BlockType::Loop {
                break;
            }

            if current_block.block_type == BlockType::ForLoop {
                break;
            }

            current_block = current_block.parent.unwrap();
            label_index += 1;
        }

        return Ok(LoInstr::Branch { label_index });
    }

    if let Some(_) = tokens.eat(Symbol, "let")?.cloned() {
        let local_name = tokens.expect_any(Symbol)?.clone();
        tokens.expect(Operator, "=")?;
        let value = parse_expr(ctx, tokens, 0)?;
        let value_type = value.get_type(ctx.module);

        if local_name.value == "_" {
            let drop_count = value_type.emit_components(&ctx.module, &mut vec![]);

            return Ok(LoInstr::Drop {
                value: Box::new(value),
                drop_count,
            });
        }

        if let Some(_) = ctx.module.globals.get(&local_name.value) {
            return Err(LoError {
                message: format!("Local name collides with global: {}", local_name.value),
                loc: local_name.loc.clone(),
            });
        };

        return define_local(ctx, &local_name, value, value_type);
    }

    if let Some(token) = tokens.peek().cloned() {
        if let Some(op) = PrefixOp::parse(token) {
            let min_bp = op.info.get_min_bp_for_next();
            tokens.next(); // skip operator

            match op.tag {
                PrefixOpTag::Not => {
                    return Ok(LoInstr::BinaryOp {
                        kind: WasmBinaryOpKind::I32_EQ,
                        lhs: Box::new(parse_expr(ctx, tokens, min_bp)?),
                        rhs: Box::new(LoInstr::U32Const { value: 0 }),
                    });
                }
                PrefixOpTag::Dereference => {
                    let pointer = Box::new(parse_expr(ctx, tokens, min_bp)?);
                    let pointer_type = pointer.get_type(ctx.module);

                    let LoType::Pointer(pointee_type) = pointer_type else {
                        return Err(LoError {
                            message: format!("Cannot dereference {pointer_type:?}"),
                            loc: op.token.loc,
                        });
                    };

                    return compile_load(ctx, &pointee_type, &pointer, 0).map_err(|err| LoError {
                        message: err,
                        loc: op.token.loc,
                    });
                }
            }
        }
    }

    let value = parse_nested_symbol(tokens)?;

    // must go first, macro values shadow locals
    if let Some(macro_args) = &ctx.block.macro_args {
        if let Some(macro_value) = macro_args.get(&value.value) {
            return Ok(macro_value.clone());
        }
    }

    if let Some(_) = tokens.eat(Operator, "!")? {
        return parse_macro_call(ctx, tokens, &value, None);
    }

    if let Some(local) = ctx.block.get_local(&value.value) {
        if ctx.module.inspect_mode {
            let value_type = &local.value_type;
            let source_index = ctx
                .module
                .included_modules
                .get(&value.loc.file_name as &str)
                .unwrap();

            let sl = value.loc.pos.line;
            let sc = value.loc.pos.col;
            let el = value.loc.end_pos.line;
            let ec = value.loc.end_pos.col;

            let local_name = &value.value;

            stdout_writeln(format!(
                "{{ \"type\": \"hover\", \
                   \"source\": {source_index}, \
                   \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                   \"content\": \"let {local_name}: {value_type}\" }}, "
            ));
        }

        return compile_local_get(&ctx.module, local.index, &local.value_type).map_err(|message| {
            LoError {
                message,
                loc: value.loc,
            }
        });
    };

    if let Some(const_value) = ctx.module.constants.borrow().get(&value.value) {
        if ctx.module.inspect_mode {
            let source_index = ctx
                .module
                .included_modules
                .get(&value.loc.file_name as &str)
                .unwrap();

            let sl = value.loc.pos.line;
            let sc = value.loc.pos.col;
            let el = value.loc.end_pos.line;
            let ec = value.loc.end_pos.col;

            let const_name = &value.value;
            let const_type = const_value.get_type(ctx.module);

            stdout_writeln(format!(
                "{{ \"type\": \"hover\", \
                   \"source\": {source_index}, \
                   \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                   \"content\": \"const {const_name}: {const_type}\" }}, "
            ));
        }

        return Ok(const_value.clone());
    }

    if let Some(global) = ctx.module.globals.get(&value.value) {
        if ctx.module.inspect_mode {
            let source_index = ctx
                .module
                .included_modules
                .get(&value.loc.file_name as &str)
                .unwrap();

            let sl = value.loc.pos.line;
            let sc = value.loc.pos.col;
            let el = value.loc.end_pos.line;
            let ec = value.loc.end_pos.col;

            let global_name = &value.value;
            let global_type = &global.value_type;

            stdout_writeln(format!(
                "{{ \"type\": \"hover\", \
                   \"source\": {source_index}, \
                   \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                   \"content\": \"let {global_name}: {global_type}\" }}, "
            ));
        }

        return Ok(LoInstr::GlobalGet {
            global_index: global.index,
        });
    };

    if let Some(fn_def) = ctx.module.fn_defs.get(&value.value) {
        let mut args = vec![];
        parse_fn_call_args(ctx, tokens, &mut args)?;
        typecheck_fn_call_args(
            ctx.module,
            &fn_def.type_.inputs,
            &args,
            &value.value,
            &value.loc,
        )?;

        if ctx.module.inspect_mode {
            let source_index = ctx
                .module
                .included_modules
                .get(&value.loc.file_name as &str)
                .unwrap();

            let sl = value.loc.pos.line;
            let sc = value.loc.pos.col;
            let el = value.loc.end_pos.line;
            let ec = value.loc.end_pos.col;

            let fn_name = &value.value;
            let params = ListDisplay(&fn_def.fn_params);
            let return_type = &fn_def.type_.output;

            stdout_writeln(format!(
                "{{ \"type\": \"hover\", \
                   \"source\": {source_index}, \
                   \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                   \"content\": \"fn {fn_name}({params}): {return_type}\" }}, "
            ));
        }

        return Ok(LoInstr::Call {
            fn_index: fn_def.get_absolute_index(ctx.module),
            return_type: fn_def.type_.output.clone(),
            args,
        });
    }

    if let Some(struct_def) = ctx.module.struct_defs.get(&value.value) {
        let struct_name = value;

        let mut values = vec![];
        tokens.expect(Delim, "{")?;
        while let None = tokens.eat(Delim, "}")? {
            let field_name = tokens.expect_any(Symbol)?.clone();
            tokens.expect(Operator, ":")?;
            let field_value_loc = tokens.loc().clone();
            let field_value = parse_expr(ctx, tokens, 0)?;

            if !tokens.next_is(Delim, "}")? {
                tokens.expect(Delim, ",")?;
            }

            let field_index = values.len();
            let Some(struct_field) = struct_def.fields.get(field_index) else {
                return Err(LoError {
                    message: format!("Excess field values"),
                    loc: field_name.loc,
                });
            };

            if &field_name.value != &struct_field.name {
                return Err(LoError {
                    message: format!("Unexpected field name, expecting: `{}`", struct_field.name),
                    loc: field_name.loc,
                });
            }

            let field_value_type = field_value.get_type(ctx.module);
            if field_value_type != struct_field.value_type {
                return Err(LoError {
                    message: format!(
                        "Invalid type for field {}.{}, expected: {}, got: {}",
                        struct_name.value,
                        field_name.value,
                        struct_field.value_type,
                        field_value_type
                    ),
                    loc: field_value_loc,
                });
            }
            values.push(field_value);
        }

        return Ok(
            LoInstr::MultiValueEmit { values }.casted(LoType::StructInstance {
                name: struct_name.value,
            }),
        );
    };

    return Err(LoError {
        message: format!("Reading unknown variable: {}", value.value),
        loc: value.loc,
    });
}

fn define_local(
    ctx: &mut BlockContext,
    local_name: &LoToken,
    value: LoInstr,
    value_type: LoType,
) -> Result<LoInstr, LoError> {
    if ctx.block.get_own_local(&local_name.value).is_some() {
        return Err(LoError {
            message: format!("Duplicate local definition: {}", local_name.value),
            loc: local_name.loc.clone(),
        });
    }

    if ctx.module.inspect_mode {
        let source_index = ctx
            .module
            .included_modules
            .get(&local_name.loc.file_name as &str)
            .unwrap();

        let sl = local_name.loc.pos.line;
        let sc = local_name.loc.pos.col;
        let el = local_name.loc.end_pos.line;
        let ec = local_name.loc.end_pos.col;

        let local_name = &local_name.value;

        stdout_writeln(format!(
            "{{ \"type\": \"hover\", \
               \"source\": {source_index}, \
               \"range\": \"{sl}:{sc}-{el}:{ec}\", \
               \"content\": \"let {local_name}: {value_type}\" }}, "
        ));
    }

    let local_index = ctx.fn_ctx.locals_last_index;
    let comp_count = value_type.emit_components(&ctx.module, &mut ctx.fn_ctx.non_arg_wasm_locals);
    ctx.fn_ctx.locals_last_index += comp_count;

    ctx.block.locals.insert(
        local_name.value.clone(),
        LocalDef {
            index: local_index,
            value_type,
        },
    );

    let local_indicies = local_index..local_index + comp_count;
    let values = local_indicies
        .map(|i| LoInstr::UntypedLocalGet { local_index: i })
        .collect();
    let bind_instr = LoInstr::MultiValueEmit { values };
    return compile_set(ctx, value, bind_instr, &local_name.loc);
}

fn parse_macro_call(
    ctx: &mut BlockContext,
    tokens: &mut LoTokenStream,
    macro_token: &LoToken,
    receiver: Option<LoInstr>,
) -> Result<LoInstr, LoError> {
    let macro_name = if let Some(receiver) = &receiver {
        let receiver_type = receiver.get_type(ctx.module);
        get_fn_name_from_method(&receiver_type, &macro_token.value)
    } else {
        macro_token.value.clone()
    };

    let Some(macro_def) = ctx.module.macros.get(&macro_name) else {
        return Err(LoError {
            message: format!("Unknown macro: {}", macro_name),
            loc: macro_token.loc.clone(),
        });
    };

    let mut type_scope = {
        let mut type_args = Vec::new();

        if let Some(_) = tokens.eat(Operator, "<")? {
            while let None = tokens.eat(Operator, ">")? {
                let macro_arg = parse_lo_type(ctx, tokens)?;
                type_args.push(macro_arg);
                if !tokens.next_is(Operator, ">")? {
                    tokens.expect(Delim, ",")?;
                }
            }
        }

        if type_args.len() != macro_def.type_params.len() {
            return Err(LoError {
                message: format!(
                    "Invalid number of type params, expected {}, got {}",
                    macro_def.type_params.len(),
                    type_args.len()
                ),
                loc: macro_token.loc.clone(),
            });
        }

        let mut type_scope = LoTypeScope::default();
        for (name, value) in macro_def.type_params.iter().zip(type_args) {
            type_scope.insert(name.clone(), value.clone());
        }

        type_scope
    };
    let return_type = macro_def.return_type.resolve_macro_type_args(&type_scope);

    let macro_args = {
        let mut args = vec![];
        if let Some(receiver) = receiver {
            args.push(receiver);
        }
        parse_fn_call_args(ctx, tokens, &mut args)?;

        let mut params = Vec::new();
        for param in &macro_def.params {
            params.push(param.type_.resolve_macro_type_args(&type_scope));
        }
        typecheck_fn_call_args(ctx.module, &params, &args, &macro_name, &macro_token.loc)?;

        let mut macro_args = BTreeMap::new();
        for (param, value) in macro_def.params.iter().zip(args) {
            macro_args.insert(param.name.clone(), value.clone());
        }

        macro_args
    };

    if let Some(parent) = &ctx.block.type_scope {
        type_scope.parent = Some(parent);
    } else {
        type_scope.parent = Some(&ctx.module.type_scope);
    }

    let macro_ctx = &mut BlockContext {
        module: ctx.module,
        fn_ctx: ctx.fn_ctx,
        block: Block {
            parent: Some(&ctx.block),
            type_scope: Some(type_scope),
            macro_args: Some(macro_args),
            ..Default::default()
        },
    };

    let exprs =
        parse_block_contents(macro_ctx, &mut macro_def.body.clone(), return_type.clone())?.exprs;

    if ctx.module.inspect_mode {
        let source_index = ctx
            .module
            .included_modules
            .get(&macro_token.loc.file_name as &str)
            .unwrap();

        let sl = macro_token.loc.pos.line;
        let sc = macro_token.loc.pos.col;
        let el = macro_token.loc.end_pos.line;
        let ec = macro_token.loc.end_pos.col;

        let params = ListDisplay(&macro_def.params);
        let type_params = ListDisplay(&macro_def.type_params);
        let return_type = &macro_def.return_type;

        stdout_writeln(format!(
            "{{ \"type\": \"hover\", \
               \"source\": {source_index}, \
               \"range\": \"{sl}:{sc}-{el}:{ec}\", \
               \"content\": \"fn {macro_name}!<{type_params}>({params}): {return_type}\" }}, "
        ));
    }

    return Ok(LoInstr::MultiValueEmit { values: exprs }.casted(return_type));
}

struct BlockContents {
    exprs: Vec<LoInstr>,
    has_never: bool,
    has_return: bool,
}

fn parse_block_contents(
    ctx: &mut BlockContext,
    tokens: &mut LoTokenStream,
    expected_type: LoType,
) -> Result<BlockContents, LoError> {
    let mut resolved_type = LoType::Void;
    let mut contents = BlockContents {
        exprs: vec![],
        has_never: false,
        has_return: false,
    };

    while tokens.peek().is_some() {
        let expr_loc = tokens.peek().unwrap().loc.clone();
        let expr = parse_expr(ctx, tokens, 0)?;
        tokens.expect(Delim, ";")?;

        let expr_type = expr.get_type(ctx.module);
        if expr_type == LoType::Never {
            contents.has_never = true;
            if let LoInstr::Return { .. } = &expr {
                contents.has_return = true;
            }
        } else if expr_type != LoType::Void {
            if expr_type != expected_type {
                return Err(LoError {
                    message: format!("Expression resolved to `{expr_type}`, but block expected `{expected_type}`"),
                    loc: expr_loc,
                });
            } else if resolved_type != LoType::Void {
                return Err(LoError {
                    message: format!(
                        "Multiple non-void expressions in the block are not supported"
                    ),
                    loc: expr_loc,
                });
            };

            resolved_type = expr_type;
        }

        contents.exprs.push(expr);
    }

    if let Some(t) = tokens.peek() {
        return Err(LoError {
            message: format!("Unexpected token at the end of block: {t:?}"),
            loc: t.loc.clone(),
        });
    }

    if !contents.has_never && resolved_type != expected_type {
        return Err(LoError {
            message: format!("Block resolved to {resolved_type} but {expected_type} was expected"),
            loc: tokens.terminal_token.loc.clone(),
        });
    }

    // This hints the wasm compilers that the block won't terminate
    if !contents.has_return && contents.has_never {
        contents.exprs.push(LoInstr::Unreachable);
    }

    Ok(contents)
}

fn build_const_str_instr(ctx: &ModuleContext, value: &str) -> LoInstr {
    let string_len = value.as_bytes().len() as u32;

    let string_ptr = ctx.string_pool.borrow().get(value).cloned();
    let string_ptr = match string_ptr {
        Some(string_ptr) => string_ptr,
        None => {
            let new_string_ptr = *ctx.data_size.borrow();
            ctx.string_pool
                .borrow_mut()
                .insert(String::from(value), new_string_ptr);

            *ctx.data_size.borrow_mut() += string_len;
            ctx.wasm_module.borrow_mut().datas.push(WasmData::Active {
                offset: WasmExpr {
                    instrs: vec![WasmInstr::I32Const {
                        value: new_string_ptr as i32,
                    }],
                },
                bytes: value.as_bytes().to_vec(),
            });
            new_string_ptr
        }
    };

    LoInstr::MultiValueEmit {
        values: vec![
            LoInstr::U32Const { value: string_ptr },
            LoInstr::U32Const { value: string_len },
        ],
    }
    .casted(LoType::StructInstance {
        name: format!("str"),
    })
}

fn parse_postfix(
    ctx: &mut BlockContext,
    tokens: &mut LoTokenStream,
    primary: LoInstr,
    mut op: InfixOp,
) -> Result<LoInstr, LoError> {
    let min_bp = op.info.get_min_bp_for_next();

    Ok(match op.tag {
        InfixOpTag::Equal
        | InfixOpTag::NotEqual
        | InfixOpTag::Less
        | InfixOpTag::Greater
        | InfixOpTag::LessEqual
        | InfixOpTag::GreaterEqual
        | InfixOpTag::Add
        | InfixOpTag::Sub
        | InfixOpTag::Mul
        | InfixOpTag::Div
        | InfixOpTag::Mod
        | InfixOpTag::And
        | InfixOpTag::Or => {
            let lhs = primary;
            let rhs = parse_expr(ctx, tokens, min_bp)?;
            LoInstr::BinaryOp {
                kind: get_binary_op(ctx.module, &op, &lhs, &rhs)?,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            }
        }
        InfixOpTag::AddAssign
        | InfixOpTag::SubAssign
        | InfixOpTag::MulAssign
        | InfixOpTag::DivAssign => {
            op.tag = get_op_additional_to_assign(&op.tag)?;

            let lhs = primary;
            let rhs = parse_expr(ctx, tokens, min_bp)?;

            let value = LoInstr::BinaryOp {
                kind: get_binary_op(ctx.module, &op, &lhs, &rhs)?,
                lhs: Box::new(lhs.clone()),
                rhs: Box::new(rhs),
            };

            compile_set(ctx, value, lhs, &op.token.loc)?
        }
        InfixOpTag::Assign => {
            let value = parse_expr(ctx, tokens, min_bp)?;
            let value_type = value.get_type(ctx.module);
            let bind_type = primary.get_type(ctx.module);

            if value_type != bind_type {
                return Err(LoError {
                    message: format!(
                        "TypeError: Invalid types for '{}', needed {bind_type}, got {value_type}",
                        op.token.value
                    ),
                    loc: op.token.loc.clone(),
                });
            }
            compile_set(ctx, value, primary, &op.token.loc)?
        }
        // TODO: support all numeric types
        InfixOpTag::Cast => {
            let actual_type = primary.get_type(ctx.module);
            let wanted_type = parse_lo_type(ctx, tokens)?;

            if wanted_type == LoType::Bool || wanted_type == LoType::I8 || wanted_type == LoType::U8
            {
                if actual_type == LoType::I32
                    || actual_type == LoType::U32
                    || actual_type == LoType::I64
                    || actual_type == LoType::U64
                {
                    return Ok(primary.casted(wanted_type));
                }
            }

            if wanted_type == LoType::I64 {
                if actual_type == LoType::I32 {
                    return Ok(LoInstr::I64FromI32Signed {
                        expr: Box::new(primary),
                    });
                }

                if actual_type == LoType::U32 {
                    return Ok(LoInstr::I64FromI32Unsigned {
                        expr: Box::new(primary),
                    });
                }
            }

            if wanted_type == LoType::U64 {
                if actual_type == LoType::I32 {
                    return Ok(LoInstr::I64FromI32Signed {
                        expr: Box::new(primary),
                    }
                    .casted(wanted_type));
                }

                if actual_type == LoType::U32 {
                    return Ok(LoInstr::I64FromI32Unsigned {
                        expr: Box::new(primary),
                    }
                    .casted(wanted_type));
                }
            }

            if wanted_type == LoType::I32 {
                if actual_type == LoType::I64 || actual_type == LoType::U64 {
                    return Ok(LoInstr::I32FromI64 {
                        expr: Box::new(primary),
                    });
                }
            }

            if wanted_type == LoType::U32 {
                if actual_type == LoType::I64 || actual_type == LoType::U64 {
                    return Ok(LoInstr::I32FromI64 {
                        expr: Box::new(primary),
                    }
                    .casted(wanted_type));
                }
            }

            let mut actual_wasm_types = vec![];
            actual_type.emit_components(ctx.module, &mut actual_wasm_types);

            let mut wanted_wasm_types = vec![];
            wanted_type.emit_components(ctx.module, &mut wanted_wasm_types);

            if actual_wasm_types != wanted_wasm_types {
                return Err(LoError {
                    message: format!("`{}` cannot be casted to `{}`", actual_type, wanted_type),
                    loc: op.token.loc,
                });
            }

            primary.casted(wanted_type)
        }
        InfixOpTag::FieldAccess => {
            let field_or_method_name = tokens.expect_any(Symbol)?.clone();
            if let Some(_) = tokens.eat(Operator, "!")? {
                return parse_macro_call(ctx, tokens, &field_or_method_name, Some(primary));
            }

            if tokens.next_is(Delim, "(").unwrap_or(false) {
                let method_name = field_or_method_name;
                let receiver_type = primary.get_type(ctx.module);

                let fn_name = get_fn_name_from_method(&receiver_type, &method_name.value);
                let Some(fn_def) = ctx.module.fn_defs.get(&fn_name) else {
                    return Err(LoError {
                        message: format!("Unknown function: {fn_name}"),
                        loc: method_name.loc,
                    });
                };

                let mut args = vec![primary];
                parse_fn_call_args(ctx, tokens, &mut args)?;
                typecheck_fn_call_args(
                    ctx.module,
                    &fn_def.type_.inputs,
                    &args,
                    &fn_name,
                    &method_name.loc,
                )?;

                if ctx.module.inspect_mode {
                    let source_index = ctx
                        .module
                        .included_modules
                        .get(&method_name.loc.file_name as &str)
                        .unwrap();

                    let sl = method_name.loc.pos.line;
                    let sc = method_name.loc.pos.col;
                    let el = method_name.loc.end_pos.line;
                    let ec = method_name.loc.end_pos.col;

                    let params = ListDisplay(&fn_def.fn_params);
                    let return_type = &fn_def.type_.output;

                    stdout_writeln(format!(
                        "{{ \"type\": \"hover\", \
                           \"source\": {source_index}, \
                           \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                           \"content\": \"fn {fn_name}({params}): {return_type}\" }}, "
                    ));
                }

                return Ok(LoInstr::Call {
                    fn_index: fn_def.get_absolute_index(ctx.module),
                    return_type: fn_def.type_.output.clone(),
                    args,
                });
            }

            let field_name = field_or_method_name;

            if let LoInstr::StructGet {
                struct_name,
                base_index,
                ..
            } = &primary
            {
                let struct_def = ctx.module.struct_defs.get(struct_name).unwrap(); // safe
                let Some(field) = struct_def
                    .fields
                    .iter()
                    .find(|f| &f.name == &field_name.value)
                else {
                    return Err(LoError {
                        message: format!(
                            "Unknown field {} in struct {struct_name}",
                            field_name.value
                        ),
                        loc: field_name.loc,
                    });
                };

                if ctx.module.inspect_mode {
                    let source_index = ctx
                        .module
                        .included_modules
                        .get(&field_name.loc.file_name as &str)
                        .unwrap();

                    let sl = field_name.loc.pos.line;
                    let sc = field_name.loc.pos.col;
                    let el = field_name.loc.end_pos.line;
                    let ec = field_name.loc.end_pos.col;

                    let field_name = &field_name.value;
                    let field_type = &field.value_type;

                    stdout_writeln(format!(
                        "{{ \"type\": \"hover\", \
                           \"source\": {source_index}, \
                           \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                           \"content\": \"{struct_name}\\n{field_name}: {field_type}\" }}, "
                    ));
                }

                return compile_local_get(
                    &ctx.module,
                    base_index + field.field_index,
                    &field.value_type,
                )
                .map_err(|message| LoError {
                    message,
                    loc: op.token.loc,
                });
            };

            if let LoInstr::StructLoad {
                struct_name,
                address_instr,
                base_byte_offset,
                ..
            } = &primary
            {
                // safe to unwrap as it was already checked in `StructLoad`
                let struct_def = ctx.module.struct_defs.get(struct_name).unwrap();

                let Some(field) = struct_def
                    .fields
                    .iter()
                    .find(|f| f.name == *field_name.value)
                else {
                    return Err(LoError {
                        message: format!(
                            "Unknown field {} in struct {struct_name}",
                            field_name.value
                        ),
                        loc: field_name.loc,
                    });
                };

                if ctx.module.inspect_mode {
                    let source_index = ctx
                        .module
                        .included_modules
                        .get(&field_name.loc.file_name as &str)
                        .unwrap();

                    let sl = field_name.loc.pos.line;
                    let sc = field_name.loc.pos.col;
                    let el = field_name.loc.end_pos.line;
                    let ec = field_name.loc.end_pos.col;

                    let field_name = &field_name.value;
                    let field_type = &field.value_type;

                    stdout_writeln(format!(
                        "{{ \"type\": \"hover\", \
                           \"source\": {source_index}, \
                           \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                           \"content\": \"{struct_name}\\n{field_name}: {field_type}\" }}, "
                    ));
                }

                return compile_load(
                    ctx,
                    &field.value_type,
                    address_instr,
                    base_byte_offset + field.byte_offset,
                )
                .map_err(|e| LoError {
                    message: e,
                    loc: op.token.loc,
                });
            }

            let primary_type = primary.get_type(ctx.module);
            if let LoType::Pointer(pointee_type) = &primary_type {
                if let LoType::StructInstance { name: struct_name } = pointee_type.as_ref() {
                    let struct_def = ctx.module.struct_defs.get(struct_name).unwrap();
                    let Some(field) = struct_def
                        .fields
                        .iter()
                        .find(|f| f.name == *field_name.value)
                    else {
                        return Err(LoError {
                            message: format!(
                                "Unknown field {} in struct {struct_name}",
                                field_name.value
                            ),
                            loc: field_name.loc.clone(),
                        });
                    };

                    if ctx.module.inspect_mode {
                        let source_index = ctx
                            .module
                            .included_modules
                            .get(&field_name.loc.file_name as &str)
                            .unwrap();

                        let sl = field_name.loc.pos.line;
                        let sc = field_name.loc.pos.col;
                        let el = field_name.loc.end_pos.line;
                        let ec = field_name.loc.end_pos.col;

                        let field_name = &field_name.value;
                        let field_type = &field.value_type;

                        stdout_writeln(format!(
                            "{{ \"type\": \"hover\", \
                               \"source\": {source_index}, \
                               \"range\": \"{sl}:{sc}-{el}:{ec}\", \
                               \"content\": \"{primary_type}\\n{field_name}: {field_type}\" }}, "
                        ));
                    }

                    return compile_load(ctx, &field.value_type, &primary, field.byte_offset)
                        .map_err(|e| LoError {
                            message: e,
                            loc: op.token.loc.clone(),
                        });
                };
            };

            return Err(LoError {
                message: format!(
                    "Trying to get field '{}' on non struct: {primary_type}",
                    field_name.value
                ),
                loc: field_name.loc,
            });
        }
        InfixOpTag::Catch => {
            let mut error_bind = tokens.expect_any(Symbol)?.clone();
            if error_bind.value == "_" {
                error_bind.value = "<ignored error>".into(); // make sure it's not accesible
            }

            let mut catch_block = collect_block_tokens(tokens)?;

            let primary_type = primary.get_type(ctx.module);
            let LoType::Result { ok_type, err_type } = primary_type else {
                return Err(LoError {
                    message: format!(
                        "Trying to catch the error from expression of type: {primary_type}",
                    ),
                    loc: op.token.loc,
                });
            };

            let catch_ctx = &mut BlockContext {
                module: ctx.module,
                fn_ctx: ctx.fn_ctx,
                block: Block {
                    parent: Some(&ctx.block),
                    ..Default::default()
                },
            };
            let catch_body = parse_block_contents(catch_ctx, &mut catch_block, *ok_type.clone())?;

            let bind_err_instr = define_local(
                catch_ctx,
                &error_bind,
                LoInstr::NoInstr, // pop error value from the stack
                *err_type.clone(),
            )?;
            let error_value = compile_local_get(
                ctx.module,
                catch_ctx
                    .block
                    .get_own_local(&error_bind.value)
                    .unwrap() // safe
                    .index,
                &err_type,
            )
            .unwrap(); // safe

            let (bind_ok_instr, ok_value) = if *ok_type != LoType::Void {
                let tmp_ok_local_name = "<ok>";
                let bind_ok_instr = define_local(
                    catch_ctx,
                    &LoToken {
                        value: tmp_ok_local_name.into(),
                        ..error_bind
                    },
                    LoInstr::NoInstr, // pop ok value from the stack
                    *ok_type.clone(),
                )?;
                let ok_value = compile_local_get(
                    ctx.module,
                    catch_ctx
                        .block
                        .get_own_local(tmp_ok_local_name)
                        .unwrap() // safe
                        .index,
                    &ok_type,
                )
                .unwrap(); // safe

                (bind_ok_instr, ok_value)
            } else {
                (LoInstr::NoInstr, LoInstr::NoInstr)
            };

            LoInstr::MultiValueEmit {
                values: vec![
                    primary,
                    bind_err_instr,
                    bind_ok_instr,
                    LoInstr::If {
                        block_type: *ok_type.clone(),
                        // TODO: this only works for WasmType::I32
                        cond: Box::new(error_value), // error_value == 0 means no error
                        then_branch: catch_body.exprs,
                        else_branch: Some(vec![ok_value]),
                    },
                ],
            }
            .casted(*ok_type.clone())
        }
    })
}

fn get_op_additional_to_assign(op: &InfixOpTag) -> Result<InfixOpTag, LoError> {
    match op {
        InfixOpTag::AddAssign => Ok(InfixOpTag::Add),
        InfixOpTag::SubAssign => Ok(InfixOpTag::Sub),
        InfixOpTag::MulAssign => Ok(InfixOpTag::Mul),
        InfixOpTag::DivAssign => Ok(InfixOpTag::Div),
        _ => return Err(LoError::unreachable(file!(), line!())),
    }
}

fn get_binary_op(
    ctx: &ModuleContext,
    op: &InfixOp,
    lhs: &LoInstr,
    rhs: &LoInstr,
) -> Result<WasmBinaryOpKind, LoError> {
    let lhs_type = lhs.get_type(ctx);
    let rhs_type = rhs.get_type(ctx);

    if lhs_type != rhs_type {
        return Err(LoError {
            message: format!(
                "Operands of `{}` have incompatible types: {} and {}",
                op.token.value, lhs_type, rhs_type
            ),
            loc: op.token.loc.clone(),
        });
    }

    Ok(match op.tag {
        InfixOpTag::Equal => match lhs_type {
            LoType::Bool | LoType::I8 | LoType::U8 | LoType::I32 | LoType::U32 => {
                WasmBinaryOpKind::I32_EQ
            }
            LoType::I64 | LoType::U64 => WasmBinaryOpKind::I64_EQ,
            LoType::F32 => WasmBinaryOpKind::F32_EQ,
            LoType::F64 => WasmBinaryOpKind::F64_EQ,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::NotEqual => match lhs_type {
            LoType::Bool | LoType::I8 | LoType::U8 | LoType::I32 | LoType::U32 => {
                WasmBinaryOpKind::I32_NE
            }
            LoType::I64 | LoType::U64 => WasmBinaryOpKind::I64_NE,
            LoType::F32 => WasmBinaryOpKind::F32_NE,
            LoType::F64 => WasmBinaryOpKind::F64_NE,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::Less => match lhs_type {
            LoType::I8 | LoType::I32 => WasmBinaryOpKind::I32_LT_S,
            LoType::Bool | LoType::U8 | LoType::U32 => WasmBinaryOpKind::I32_LT_U,
            LoType::I64 => WasmBinaryOpKind::I64_LT_S,
            LoType::U64 => WasmBinaryOpKind::I64_LT_U,
            LoType::F32 => WasmBinaryOpKind::F32_LT,
            LoType::F64 => WasmBinaryOpKind::F64_LT,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::Greater => match lhs_type {
            LoType::I8 | LoType::I32 => WasmBinaryOpKind::I32_GT_S,
            LoType::Bool | LoType::U8 | LoType::U32 => WasmBinaryOpKind::I32_GT_U,
            LoType::I64 => WasmBinaryOpKind::I64_GT_S,
            LoType::U64 => WasmBinaryOpKind::I64_GT_U,
            LoType::F32 => WasmBinaryOpKind::F32_GT,
            LoType::F64 => WasmBinaryOpKind::F64_GT,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::LessEqual => match lhs_type {
            LoType::I8 | LoType::I32 => WasmBinaryOpKind::I32_LE_S,
            LoType::Bool | LoType::U8 | LoType::U32 => WasmBinaryOpKind::I32_LE_U,
            LoType::I64 => WasmBinaryOpKind::I64_LE_S,
            LoType::U64 => WasmBinaryOpKind::I64_LE_U,
            LoType::F32 => WasmBinaryOpKind::F32_LE,
            LoType::F64 => WasmBinaryOpKind::F64_LE,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::GreaterEqual => match lhs_type {
            LoType::I8 | LoType::I32 => WasmBinaryOpKind::I32_GE_S,
            LoType::Bool | LoType::U8 | LoType::U32 => WasmBinaryOpKind::I32_GE_U,
            LoType::I64 => WasmBinaryOpKind::I64_GE_S,
            LoType::U64 => WasmBinaryOpKind::I64_GE_U,
            LoType::F32 => WasmBinaryOpKind::F32_GE,
            LoType::F64 => WasmBinaryOpKind::F64_GE,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::Add => match lhs_type {
            LoType::Bool | LoType::I8 | LoType::U8 | LoType::I32 | LoType::U32 => {
                WasmBinaryOpKind::I32_ADD
            }
            LoType::I64 | LoType::U64 => WasmBinaryOpKind::I64_ADD,
            LoType::F32 => WasmBinaryOpKind::F32_ADD,
            LoType::F64 => WasmBinaryOpKind::F64_ADD,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::Sub => match lhs_type {
            LoType::Bool | LoType::I8 | LoType::U8 | LoType::I32 | LoType::U32 => {
                WasmBinaryOpKind::I32_SUB
            }
            LoType::I64 | LoType::U64 => WasmBinaryOpKind::I64_SUB,
            LoType::F32 => WasmBinaryOpKind::F32_SUB,
            LoType::F64 => WasmBinaryOpKind::F64_SUB,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::Mul => match lhs_type {
            LoType::Bool | LoType::I8 | LoType::U8 | LoType::I32 | LoType::U32 => {
                WasmBinaryOpKind::I32_MUL
            }
            LoType::I64 | LoType::U64 => WasmBinaryOpKind::I64_MUL,
            LoType::F32 => WasmBinaryOpKind::F32_MUL,
            LoType::F64 => WasmBinaryOpKind::F64_MUL,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::Div => match lhs_type {
            LoType::I8 | LoType::I32 => WasmBinaryOpKind::I32_DIV_S,
            LoType::Bool | LoType::U8 | LoType::U32 => WasmBinaryOpKind::I32_DIV_U,
            LoType::I64 => WasmBinaryOpKind::I64_DIV_S,
            LoType::U64 => WasmBinaryOpKind::I64_DIV_U,
            LoType::F32 => WasmBinaryOpKind::F32_DIV,
            LoType::F64 => WasmBinaryOpKind::F64_DIV,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::Mod => match lhs_type {
            LoType::I8 | LoType::I32 => WasmBinaryOpKind::I32_REM_S,
            LoType::Bool | LoType::U8 | LoType::U32 => WasmBinaryOpKind::I32_REM_U,
            LoType::I64 => WasmBinaryOpKind::I64_REM_S,
            LoType::U64 => WasmBinaryOpKind::I64_REM_U,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::And => match lhs_type {
            LoType::Bool | LoType::I8 | LoType::U8 | LoType::I32 | LoType::U32 => {
                WasmBinaryOpKind::I32_AND
            }
            LoType::I64 | LoType::U64 => WasmBinaryOpKind::I64_AND,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        InfixOpTag::Or => match lhs_type {
            LoType::Bool | LoType::I8 | LoType::U8 | LoType::I32 | LoType::U32 => {
                WasmBinaryOpKind::I32_OR
            }
            LoType::I64 | LoType::U64 => WasmBinaryOpKind::I64_OR,
            operand_type => return err_incompatible_op(op, operand_type),
        },
        _ => return Err(LoError::unreachable(file!(), line!())),
    })
}

fn err_incompatible_op<T>(op: &InfixOp, operand_type: LoType) -> Result<T, LoError> {
    Err(LoError {
        message: format!(
            "Operator `{}` is incompatible with operands of type {}",
            op.token.value, operand_type
        ),
        loc: op.token.loc.clone(),
    })
}

fn parse_fn_call_args(
    ctx: &mut BlockContext,
    tokens: &mut LoTokenStream,
    args: &mut Vec<LoInstr>,
) -> Result<(), LoError> {
    tokens.expect(Delim, "(")?;
    while let None = tokens.eat(Delim, ")")? {
        args.push(parse_expr(ctx, tokens, 0)?);

        if !tokens.next_is(Delim, ")")? {
            tokens.expect(Delim, ",")?;
        }
    }

    Ok(())
}

fn typecheck_fn_call_args(
    ctx: &ModuleContext,
    params: &Vec<LoType>,
    args: &Vec<LoInstr>,
    fn_name: &str,
    fn_call_loc: &LoLocation,
) -> Result<(), LoError> {
    let mut arg_types = vec![];
    for arg in args {
        arg_types.push(arg.get_type(ctx));
    }

    if arg_types != *params {
        return Err(LoError {
            message: format!(
                "Invalid arguments for `{}` call: [{}], expected: [{}]",
                fn_name,
                ListDisplay(&arg_types),
                ListDisplay(params)
            ),
            loc: fn_call_loc.clone(),
        });
    }

    Ok(())
}

fn parse_const_expr(
    ctx: &ModuleContext,
    tokens: &mut LoTokenStream,
    min_bp: u32,
) -> Result<LoInstr, LoError> {
    let mut primary = parse_const_primary(ctx, tokens)?;

    while tokens.peek().is_some() {
        let op_symbol = tokens.peek().unwrap().clone();
        let Some(op) = InfixOp::parse(op_symbol) else {
            break;
        };

        if op.info.bp < min_bp {
            break;
        }

        tokens.next(); // skip operator
        primary = parse_const_postfix(ctx, tokens, primary, op)?;
    }

    Ok(primary)
}

fn parse_const_primary(
    ctx: &ModuleContext,
    tokens: &mut LoTokenStream,
) -> Result<LoInstr, LoError> {
    if tokens.next_is_any(IntLiteral)? {
        return parse_const_int(tokens);
    }

    if let Some(value) = tokens.eat_any(CharLiteral)? {
        return Ok(LoInstr::U32Const {
            value: value.value.chars().next().unwrap() as u32,
        });
    }

    if let Some(value) = tokens.eat_any(StringLiteral)? {
        return Ok(build_const_str_instr(ctx, &value.value));
    }

    if let Some(_) = tokens.eat(Symbol, "true")? {
        return Ok(LoInstr::U32Const { value: 1 }.casted(LoType::Bool));
    }

    if let Some(_) = tokens.eat(Symbol, "false")? {
        return Ok(LoInstr::U32Const { value: 0 }.casted(LoType::Bool));
    }

    if let Some(_) = tokens.eat(Symbol, "__DATA_SIZE__")? {
        return Ok(LoInstr::U32ConstLazy {
            value: ctx.data_size.clone(),
        });
    }

    let value = parse_nested_symbol(tokens)?;

    if let Some(const_value) = ctx.constants.borrow().get(&value.value) {
        return Ok(const_value.clone());
    }

    let Some(global) = ctx.globals.get(&value.value) else {
        return Err(LoError {
            message: format!("Reading unknown variable in const context: {}", value.value),
            loc: value.loc,
        });
    };

    return Ok(LoInstr::GlobalGet {
        global_index: global.index,
    });
}

fn parse_const_postfix(
    ctx: &ModuleContext,
    tokens: &mut LoTokenStream,
    primary: LoInstr,
    op: InfixOp,
) -> Result<LoInstr, LoError> {
    let _min_bp = op.info.get_min_bp_for_next();

    Ok(match op.tag {
        // TODO: use cast logic from `parse_postfix`
        InfixOpTag::Cast => primary.casted(parse_const_lo_type(ctx, tokens)?),
        _ => {
            return Err(LoError {
                message: format!("Unsupported operator in const context: {}", op.token.value),
                loc: op.token.loc,
            });
        }
    })
}

fn parse_const_lo_type(ctx: &ModuleContext, tokens: &mut LoTokenStream) -> Result<LoType, LoError> {
    parse_lo_type_(ctx, &ctx.type_scope, tokens, false)
}

fn parse_lo_type(ctx: &BlockContext, tokens: &mut LoTokenStream) -> Result<LoType, LoError> {
    if let Some(type_scope) = &ctx.block.type_scope {
        parse_lo_type_(ctx.module, &type_scope, tokens, false)
    } else {
        parse_const_lo_type(ctx.module, tokens)
    }
}

fn parse_lo_type_(
    ctx: &ModuleContext,
    type_scope: &LoTypeScope,
    tokens: &mut LoTokenStream,
    is_referenced: bool,
) -> Result<LoType, LoError> {
    if let Some(_) = tokens.eat(Operator, "&")? {
        let pointee = parse_lo_type_(ctx, type_scope, tokens, true)?;
        return Ok(LoType::Pointer(Box::new(pointee)));
    }

    if let Some(_) = tokens.eat(Operator, "&*")? {
        let pointee = parse_lo_type_(ctx, type_scope, tokens, true)?;
        return Ok(LoType::Pointer(Box::new(pointee)));
    }

    let token = parse_nested_symbol(tokens)?;
    let type_ = get_type_by_name(ctx, type_scope, &token, is_referenced)?;

    if let Some(_) = tokens.eat(Symbol, "throws")? {
        let error_type = parse_lo_type_(ctx, type_scope, tokens, is_referenced)?;

        return Ok(LoType::Result {
            ok_type: Box::new(type_),
            err_type: Box::new(error_type),
        });
    }

    return Ok(type_);
}

fn get_type_by_name(
    ctx: &ModuleContext,
    type_scope: &LoTypeScope,
    token: &LoToken,
    is_referenced: bool,
) -> Result<LoType, LoError> {
    match token.value.as_str() {
        "never" => Ok(LoType::Never),
        "void" => Ok(LoType::Void),
        "bool" => Ok(LoType::Bool),
        "u8" => Ok(LoType::U8),
        "i8" => Ok(LoType::I8),
        "u16" => Ok(LoType::U16),
        "i16" => Ok(LoType::I16),
        "u32" => Ok(LoType::U32),
        "i32" => Ok(LoType::I32),
        "f32" => Ok(LoType::F32),
        "u64" => Ok(LoType::U64),
        "i64" => Ok(LoType::I64),
        "f64" => Ok(LoType::F64),
        _ => {
            let Some(type_) = type_scope.get(&token.value) else {
                return Err(LoError {
                    message: format!("Unknown type: {}", token.value),
                    loc: token.loc.clone(),
                });
            };

            if let LoType::StructInstance { name } = type_ {
                let struct_def = ctx.struct_defs.get(name).unwrap(); // safe because of if let
                if !struct_def.fully_defined && !is_referenced {
                    return Err(LoError {
                        message: format!("Cannot use partially defined struct: {name}"),
                        loc: token.loc.clone(),
                    });
                }
            }

            return Ok(type_.clone());
        }
    }
}

fn parse_nested_symbol(tokens: &mut LoTokenStream) -> Result<LoToken, LoError> {
    let mut nested_symbol = tokens.expect_any(Symbol)?.clone();
    while let Some(_) = tokens.eat(Operator, "::")? {
        let path_part = tokens.expect_any(Symbol)?;
        nested_symbol.value += "::";
        nested_symbol.value += path_part.value.as_str();
        nested_symbol.loc.end_pos = path_part.loc.end_pos.clone();
    }
    Ok(nested_symbol)
}

fn extract_method_receiver_and_name(
    ctx: &ModuleContext,
    token: &LoToken,
) -> Result<(Option<LoType>, String), LoError> {
    Ok(match token.value.rsplitn(2, "::").collect::<Vec<_>>()[..] {
        [method_name, receiver_name] => {
            let mut token = LoToken {
                type_: LoTokenType::Symbol,
                value: String::from(receiver_name),
                loc: token.loc.clone(),
            };

            // TODO: correct `end_pos` info is lost during creation of nested_symbol
            token.loc.end_pos = token.loc.pos.clone();

            (
                Some(get_type_by_name(ctx, &ctx.type_scope, &token, false)?),
                String::from(method_name),
            )
        }
        [fn_name] => (None, String::from(fn_name)),
        _ => unreachable!(),
    })
}

fn parse_u32_literal(int: &LoToken) -> Result<u32, LoError> {
    int.value.parse().map_err(|_| LoError {
        message: format!("Parsing u32 (implicit) failed"),
        loc: int.loc.clone(),
    })
}

fn parse_const_int(tokens: &mut LoTokenStream) -> Result<LoInstr, LoError> {
    let int_literal = tokens.expect_any(IntLiteral)?.clone();

    if let Some(_) = tokens.eat(Symbol, "i64")? {
        return Ok(LoInstr::I64Const {
            value: parse_i64_literal(&int_literal)?,
        });
    }

    if let Some(_) = tokens.eat(Symbol, "u64")? {
        return Ok(LoInstr::U64Const {
            value: parse_u64_literal(&int_literal)?,
        });
    }

    return Ok(LoInstr::U32Const {
        value: parse_u32_literal(&int_literal)?,
    });
}

fn parse_i64_literal(int: &LoToken) -> Result<i64, LoError> {
    int.value.parse().map_err(|_| LoError {
        message: format!("Parsing i64 failed"),
        loc: int.loc.clone(),
    })
}

fn parse_u64_literal(int: &LoToken) -> Result<u64, LoError> {
    int.value.parse().map_err(|_| LoError {
        message: format!("Parsing u64 failed"),
        loc: int.loc.clone(),
    })
}

fn get_fn_name_from_method(receiver_type: &LoType, method_name: &str) -> String {
    let resolved_receiver_type = receiver_type.deref_rec();
    format!("{resolved_receiver_type}::{method_name}")
}

fn get_deferred(ctx: &mut BlockContext) -> Option<Vec<LoInstr>> {
    if ctx.fn_ctx.defers.len() == 0 {
        return None;
    };

    let mut deferred = ctx.fn_ctx.defers.clone();
    deferred.reverse();

    Some(deferred)
}

fn compile_load(
    ctx: &mut BlockContext,
    value_type: &LoType,
    address_instr: &LoInstr,
    base_byte_offset: u32,
) -> Result<LoInstr, String> {
    if let Ok(_) = value_type.to_load_kind() {
        return Ok(LoInstr::Load {
            kind: value_type.clone(),
            align: 0,
            offset: base_byte_offset,
            address_instr: Box::new(address_instr.clone()),
        });
    }

    if let LoType::Tuple(item_types) = value_type {
        let mut item_gets = vec![];
        let mut item_byte_offset = 0;
        for item_type in item_types {
            item_gets.push(compile_load(
                ctx,
                item_type,
                address_instr,
                base_byte_offset + item_byte_offset,
            )?);
            item_byte_offset += item_type.sized_comp_stats(&ctx.module)?.byte_length;
        }

        return Ok(LoInstr::MultiValueEmit { values: item_gets }.casted(value_type.clone()));
    }

    let LoType::StructInstance { name } = value_type else {
        return Err(format!("Unsupported type for compile_load: {value_type:?}"));
    };

    let mut components = vec![];
    let mut stats = EmitComponentStats {
        count: 0,
        byte_length: base_byte_offset,
    };

    value_type.emit_sized_component_stats(&ctx.module, &mut stats, &mut components)?;

    let address_local_index = ctx.fn_ctx.locals_last_index;
    ctx.fn_ctx.non_arg_wasm_locals.push(WasmType::I32);
    ctx.fn_ctx.locals_last_index += 1;

    let mut primitive_loads = vec![];
    for comp in components.into_iter() {
        primitive_loads.push(LoInstr::Load {
            kind: comp.value_type,
            align: 1,
            offset: comp.byte_offset,
            address_instr: Box::new(LoInstr::UntypedLocalGet {
                local_index: address_local_index,
            }),
        });
    }

    Ok(LoInstr::StructLoad {
        struct_name: name.clone(),
        address_instr: Box::new(address_instr.clone()),
        address_local_index,
        base_byte_offset,
        primitive_loads,
    })
}

fn compile_local_get(
    ctx: &ModuleContext,
    base_index: u32,
    value_type: &LoType,
) -> Result<LoInstr, String> {
    if let LoType::Tuple(item_types) = value_type {
        let mut item_gets = vec![];
        for (item_index, item_type) in (0..).zip(item_types) {
            item_gets.push(compile_local_get(ctx, base_index + item_index, item_type)?);
        }

        return Ok(LoInstr::MultiValueEmit { values: item_gets }.casted(value_type.clone()));
    }

    let comp_count = value_type.emit_components(ctx, &mut vec![]);

    let LoType::StructInstance { name } = value_type else {
        if comp_count == 1 {
            return Ok(LoInstr::LocalGet {
                local_index: base_index,
                value_type: value_type.clone(),
            });
        }

        return Err(format!("Unsupported type for compile_load: {value_type:?}"));
    };

    let mut primitive_gets = vec![];
    for field_index in 0..comp_count {
        primitive_gets.push(LoInstr::UntypedLocalGet {
            local_index: base_index + field_index as u32,
        });
    }

    Ok(LoInstr::StructGet {
        struct_name: name.clone(),
        base_index,
        primitive_gets,
    })
}

fn compile_set(
    ctx: &mut BlockContext,
    value_instr: LoInstr,
    bind_instr: LoInstr,
    loc: &LoLocation,
) -> Result<LoInstr, LoError> {
    let mut values = vec![];
    compile_set_binds(&mut values, ctx, bind_instr, None).map_err(|message| LoError {
        message,
        loc: loc.clone(),
    })?;
    values.push(value_instr);
    values.reverse();

    Ok(LoInstr::MultiValueEmit { values }.casted(LoType::Void))
}

fn compile_set_binds(
    output: &mut Vec<LoInstr>,
    ctx: &mut BlockContext,
    bind_instr: LoInstr,
    address_index: Option<u32>,
) -> Result<(), String> {
    Ok(match bind_instr {
        LoInstr::LocalGet { local_index, .. } | LoInstr::UntypedLocalGet { local_index } => {
            output.push(LoInstr::Set {
                bind: LoSetBind::Local { index: local_index },
            });
        }
        LoInstr::GlobalGet { global_index } => {
            output.push(LoInstr::Set {
                bind: LoSetBind::Global {
                    index: global_index,
                },
            });
        }
        LoInstr::Load {
            kind,
            align,
            offset,
            address_instr,
        } => {
            let value_local_index = ctx.fn_ctx.locals_last_index;
            ctx.fn_ctx
                .non_arg_wasm_locals
                .push(kind.to_wasm_type().unwrap());
            ctx.fn_ctx.locals_last_index += 1;

            let address_instr = match address_index {
                Some(local_index) => Box::new(LoInstr::UntypedLocalGet { local_index }),
                None => address_instr,
            };

            output.push(LoInstr::Set {
                bind: LoSetBind::Memory {
                    align,
                    offset,
                    kind: WasmStoreKind::from_load_kind(&kind.to_load_kind().unwrap()),
                    address_instr,
                    value_local_index,
                },
            });
        }
        LoInstr::StructLoad {
            primitive_loads,
            address_instr,
            address_local_index,
            ..
        } => {
            let mut values = vec![];

            for value in primitive_loads {
                compile_set_binds(&mut values, ctx, value, Some(address_local_index))?;
            }

            values.push(LoInstr::Set {
                bind: LoSetBind::Local {
                    index: address_local_index,
                },
            });
            values.push(*address_instr);

            values.reverse();

            output.push(LoInstr::MultiValueEmit { values });
        }
        LoInstr::StructGet { primitive_gets, .. } => {
            for value in primitive_gets {
                compile_set_binds(output, ctx, value, address_index)?;
            }
        }
        LoInstr::MultiValueEmit { values } => {
            for value in values {
                compile_set_binds(output, ctx, value, address_index)?;
            }
        }
        LoInstr::Casted { expr, .. } => {
            compile_set_binds(output, ctx, *expr, address_index)?;
        }
        _ => {
            return Err(format!("Invalid left-hand side in assignment"));
        }
    })
}
