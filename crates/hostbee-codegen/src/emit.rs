//! 命令模型构建与产物文本生成。
//!
//! 输入 [`crate::schema::SchemaModel`]，输出两个产物文件的完整文本：
//! - `docs.rs`：每个 root field 的 GraphQL document 常量（depth 0..=8 每档一份 + 拼接前缀）；
//! - `mod.rs`：全量命令注册表（`FieldSpec` 静态表，类型定义在 hostbee 的 `commands` 模块）。
//!
//! 产物确定性：按领域组固定顺序 + 组内 schema 原序排序，文本逐字节稳定（重跑幂等）。

use std::collections::BTreeSet;

use crate::domain;
use crate::expand::{DEFAULT_DEPTH, DEPTH_COUNT, Expander};
use crate::schema::SchemaModel;

/// 不生成子命令的 root field（CLI 手写实现，见 codegen-design.md §5）。
pub const SKIP_FIELDS: &[&str] = &["login", "refresh"];

/// 参数 → flag 的种类（生成侧模型；hostbee 侧对应 `ArgKind`）。
pub enum ArgKindEmit {
    Int,
    Float,
    Boolean,
    Str,
    /// enum 标量：值为 schema 枚举名。
    Enum(Vec<String>),
    /// JSON 透传：输入对象（含 oneOf）/ JSON / Money / BorshOrJson / 输入对象列表。
    Json,
    /// `[Int!]` 等标量列表。
    IntList,
    /// `[String!]` 列表。
    StrList,
}

impl ArgKindEmit {
    fn rust_text(&self) -> String {
        match self {
            ArgKindEmit::Int => "ArgKind::Int".to_owned(),
            ArgKindEmit::Float => "ArgKind::Float".to_owned(),
            ArgKindEmit::Boolean => "ArgKind::Boolean".to_owned(),
            ArgKindEmit::Str => "ArgKind::Str".to_owned(),
            ArgKindEmit::Json => "ArgKind::Json".to_owned(),
            ArgKindEmit::IntList => "ArgKind::IntList".to_owned(),
            ArgKindEmit::StrList => "ArgKind::StrList".to_owned(),
            ArgKindEmit::Enum(values) => format!(
                "ArgKind::Enum(&[{}])",
                values
                    .iter()
                    .map(|v| rust_str(v))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// 一个 root field 对应的完整命令模型（产物文本的原料）。
pub struct CommandModel {
    pub domain: &'static str,
    pub field: String,
    /// kebab-case 子命令名。
    pub command: String,
    /// 产物常量名的 SCREAMING_SNAKE 前缀。
    pub const_name: String,
    pub mutation: bool,
    /// 返回值是否对象类型（决定 `--depth`/`--fields` 是否挂载）。
    pub returns_object: bool,
    /// operation 头 + field + 实参（`--fields` 覆盖时拼接用）。
    pub prefix: String,
    /// depth 0..=8 每档一份完整 document。
    pub documents: Vec<String>,
    /// 子命令 about（schema docstring 优先，缺省用 GraphQL 签名）。
    pub about: String,
    pub args: Vec<ArgEmit>,
}

pub struct ArgEmit {
    pub flag: String,
    /// GraphQL 参数名（variables 键）。
    pub arg: String,
    pub kind: ArgKindEmit,
    /// GraphQL 侧必填（非 null 输入位置）。
    pub required: bool,
    /// clap default_value 文本（JSON 字面量：`25` / `{}` / `[]`）。
    pub default: Option<String>,
    pub help: String,
}

/// camelCase → PascalCase（首字母大写；`vmInstances` → `VmInstances`）。
pub fn pascal(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// camelCase → kebab-case（`vmInstancesConnection` → `vm-instances-connection`）。
pub fn kebab(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.char_indices() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// camelCase → SCREAMING_SNAKE（`backendS3Test` → `BACKEND_S3_TEST`）。
pub fn scream(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.char_indices() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c);
        } else {
            out.push(c.to_ascii_uppercase());
        }
    }
    out
}

/// Rust 字符串字面量（Debug 转义，可打印非 ASCII 原样保留）。
fn rust_str(s: &str) -> String {
    format!("{s:?}")
}

/// 构建 all 命令模型：覆盖全部 root field，跳过手写例外（login/refresh）。
pub fn build_commands(schema: &SchemaModel) -> Result<Vec<CommandModel>, String> {
    let expander = Expander::new(schema);
    let mut commands = Vec::new();
    for rf in &schema.root_fields {
        if SKIP_FIELDS.contains(&rf.name.as_str()) {
            continue;
        }
        let domain = domain::assign(&rf.name)?;
        let args = rf
            .args
            .iter()
            .map(|arg| build_arg(schema, arg.name.as_str(), arg))
            .collect::<Result<Vec<_>, String>>()?;
        // operation 头：变量声明取 schema 原始类型文本。
        let op = if rf.mutation { "mutation" } else { "query" };
        // operation 名 PascalCase 化：HostbeeVmInstances（与字段一一对应，全局唯一）。
        let op_name = format!("Hostbee{}", pascal(&rf.name));
        let var_defs = rf
            .args
            .iter()
            .map(|a| format!("${}: {}", a.name, a.ty.render()))
            .collect::<Vec<_>>()
            .join(", ");
        let head = if var_defs.is_empty() {
            format!("{op} {op_name} {{ ")
        } else {
            format!("{op} {op_name}({var_defs}) {{ ")
        };
        let arg_refs = rf
            .args
            .iter()
            .map(|a| format!("{}: ${}", a.name, a.name))
            .collect::<Vec<_>>()
            .join(", ");
        let prefix = if arg_refs.is_empty() {
            format!("{head}{}", rf.name)
        } else {
            format!("{head}{}({arg_refs})", rf.name)
        };
        let returns_object = schema.objects.contains_key(rf.ret.named());
        let documents = (0..DEPTH_COUNT)
            .map(
                |depth| match expander.root_selection(rf.ret.named(), depth) {
                    Some(sel) => format!("{prefix} {sel} }}"),
                    None => format!("{prefix} }}"),
                },
            )
            .collect::<Vec<_>>();
        // 子命令 about：schema docstring 优先，缺省回退 GraphQL 签名。
        let arg_names = rf
            .args
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let about = if rf.doc.is_empty() {
            format!("GraphQL: {}({arg_names}): {}", rf.name, rf.ret.render())
        } else {
            rf.doc.clone()
        };
        commands.push(CommandModel {
            domain,
            field: rf.name.clone(),
            command: kebab(&rf.name),
            const_name: scream(&rf.name),
            mutation: rf.mutation,
            returns_object,
            prefix,
            documents,
            about,
            args,
        });
    }
    // 排序：领域组固定顺序；同组内保持 schema 原序（Query 原序在前，Mutation 原序在后）。
    let domain_index = |name: &str| {
        domain::DOMAIN_ORDER
            .iter()
            .position(|d| *d == name)
            .unwrap_or(usize::MAX)
    };
    commands.sort_by_key(|c| domain_index(c.domain));
    // 常量名唯一性（字段名全局唯一，此处防御性断言）。
    let mut seen = BTreeSet::new();
    for c in &commands {
        if !seen.insert(c.const_name.as_str()) {
            return Err(format!("产物常量名冲突：{}", c.const_name));
        }
    }
    Ok(commands)
}

/// 参数 → flag 模型（codegen-design.md §3 约定）。
fn build_arg(
    schema: &SchemaModel,
    field: &str,
    arg: &crate::schema::ArgModel,
) -> Result<ArgEmit, String> {
    let flag = kebab(&arg.name);
    let (kind, json_hint) = classify(schema, field, arg)?;
    let required = arg.ty.required();
    // 必填输入对象/JSON 一律给空 JSON 默认值（实测 `filter: {}`、`filters: []` 可跑，
    // 允许省略 flag）；分页参数按档位风格给默认（codegen-design.md §3.3）。
    let mut default = match (&kind, required) {
        (ArgKindEmit::Json, true) => Some(if arg.ty.is_list() { "[]" } else { "{}" }.to_owned()),
        _ => None,
    };
    if let ArgKindEmit::Int = kind {
        match arg.name.as_str() {
            "pageSize" => default = Some("25".to_owned()),
            "pageNum" => default = Some("1".to_owned()),
            "first" => default = Some("25".to_owned()),
            _ => {}
        }
    }
    let list_hint = matches!(kind, ArgKindEmit::IntList | ArgKindEmit::StrList);
    let mut help = if arg.doc.is_empty() {
        format!("类型 {}", arg.ty.render())
    } else {
        arg.doc.clone()
    };
    if let Some(hint) = json_hint {
        help.push_str(hint);
    }
    if list_hint {
        help.push_str("（逗号分隔或重复传值）");
    }
    Ok(ArgEmit {
        flag,
        arg: arg.name.clone(),
        kind,
        required,
        default,
        help,
    })
}

/// 参数类型 → flag 种类。未知自定义标量 fail-fast（显式映射表之外的一律报错）。
fn classify(
    schema: &SchemaModel,
    field: &str,
    arg: &crate::schema::ArgModel,
) -> Result<(ArgKindEmit, Option<&'static str>), String> {
    let ty = &arg.ty;
    if ty.is_list() {
        return Ok(match ty.named() {
            "Int" => (ArgKindEmit::IntList, None),
            "String" => (ArgKindEmit::StrList, None),
            _ => (ArgKindEmit::Json, Some("（JSON 数组透传）")),
        });
    }
    let name = ty.named();
    let kind = match name {
        "Int" => ArgKindEmit::Int,
        "Float" => ArgKindEmit::Float,
        "Boolean" => ArgKindEmit::Boolean,
        "String" | "ID" | "DateTime" | "Upload" => ArgKindEmit::Str,
        // 结构化自定义标量：整个 JSON 值即标量值，按 JSON 透传。
        "JSON" | "Money" | "PasskeyCredential" | "RegisterPasskeyCredential" => {
            return Ok((ArgKindEmit::Json, Some("（JSON 透传）")));
        }
        _ => {
            if let Some(values) = schema.enums.get(name) {
                ArgKindEmit::Enum(values.clone())
            } else if schema.inputs.contains_key(name) {
                let hint = if schema.one_of.contains(name) {
                    "（JSON 透传，oneOf：恰好传一个键）"
                } else {
                    "（JSON 透传）"
                };
                return Ok((ArgKindEmit::Json, Some(hint)));
            } else if schema.scalars.contains(name) {
                return Err(format!(
                    "root field {field} 的参数 {} 用了未映射的自定义标量 {name}：\
                     请在 hostbee-codegen 的 emit.rs 增补映射",
                    arg.name
                ));
            } else if schema.objects.contains_key(name) {
                return Err(format!(
                    "root field {field} 的参数 {} 的类型 {name} 是输出对象（不可能合法）",
                    arg.name
                ));
            } else {
                return Err(format!(
                    "root field {field} 的参数 {} 引用了未定义类型 {name}",
                    arg.name
                ));
            }
        }
    };
    Ok((kind, None))
}

/// 生成 docs.rs 全文。
pub fn emit_docs(commands: &[CommandModel], schema_sha256: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "//! hostbee 命令面生成产物：每个 root field 的 GraphQL document 常量。\n\
         //!\n\
         //! 由 `cargo run -p hostbee-codegen` 从 vendored schema 生成，请勿手改；重跑幂等。\n\
         //! schema 来源：vendor/backend/rustybee/schema.graphql\n\
         //! schema sha256: {schema_sha256}\n\
         //!\n\
         //! 每个 field 两件套（`<FIELD>` 为 SCREAMING_SNAKE 形式的字段名）：\n\
         //! - `<FIELD>_PREFIX`：operation 头 + field + 实参，`--fields` 覆盖 selection set 时拼接；\n\
         //! - `<FIELD>_DOCS`：depth 0..=8 每档一份完整 document，默认档位 {DEFAULT_DEPTH}。\n\n"
    ));
    for c in commands {
        out.push_str("#[rustfmt::skip]\n");
        out.push_str(&format!(
            "pub const {}_PREFIX: &str = {};\n\n",
            c.const_name,
            rust_str(&c.prefix)
        ));
        if c.returns_object {
            out.push_str("#[rustfmt::skip]\n");
            let docs = c
                .documents
                .iter()
                .map(|d| format!("    {},", rust_str(d)))
                .collect::<Vec<_>>()
                .join("\n");
            out.push_str(&format!(
                "pub static {}_DOCS: &[&str] = &[\n{docs}\n];\n\n",
                c.const_name
            ));
        } else {
            // 标量返回：九档同一份 document（无 selection set，深度无关）。
            out.push_str("#[rustfmt::skip]\n");
            out.push_str(&format!(
                "pub static {}_DOCS: &[&str] = &[{}; {}];\n\n",
                c.const_name,
                rust_str(&c.documents[0]),
                DEPTH_COUNT
            ));
        }
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// 生成 mod.rs 全文。
pub fn emit_mod(commands: &[CommandModel], schema_sha256: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "//! hostbee 命令面生成产物：全量命令注册表。\n\
         //!\n\
         //! 由 `cargo run -p hostbee-codegen` 从 vendored schema 生成，请勿手改；重跑幂等。\n\
         //! 覆盖全部 root field，`login`/`refresh` 除外（auth.rs 手写实现，codegen-design.md §5）。\n\
         //! 领域分组规则：spec-notes/schema-survey.md §1；展开算法：codegen-design.md §1.1。\n\
         //! schema sha256: {schema_sha256}\n\n\
         pub mod docs;\n\n\
         use crate::commands::{{ArgKind, ArgSpec, FieldSpec}};\n\n\
         /// 当前 schema 的 sha256 漂移标记（unit 测试校验；不一致 = schema 变了请重跑生成器）。\n\
         pub const SCHEMA_SHA256: &str = {sha};\n\n\
         /// 全量命令注册表：领域组固定顺序，组内 Query 原序在前、Mutation 原序在后。\n\
         /// 运行时接线见 `crate::commands`（ENABLED_DOMAINS 控制哪些组挂进 CLI）。\n\
         #[rustfmt::skip]\n\
         pub static FIELDS: &[FieldSpec] = &[\n",
        sha = rust_str(schema_sha256),
    ));
    for c in commands {
        out.push_str(&field_spec_text(c));
    }
    out.push_str("];\n");
    out
}

/// 单个 FieldSpec 的文本。
fn field_spec_text(c: &CommandModel) -> String {
    let mut out = String::new();
    out.push_str("    FieldSpec {\n");
    out.push_str(&format!("        domain: {},\n", rust_str(c.domain)));
    out.push_str(&format!("        command: {},\n", rust_str(&c.command)));
    out.push_str(&format!("        field: {},\n", rust_str(&c.field)));
    out.push_str(&format!("        mutation: {},\n", c.mutation));
    out.push_str(&format!("        returns_object: {},\n", c.returns_object));
    out.push_str(&format!("        prefix: docs::{}_PREFIX,\n", c.const_name));
    out.push_str(&format!(
        "        documents: docs::{}_DOCS,\n",
        c.const_name
    ));
    out.push_str(&format!("        about: {},\n", rust_str(&c.about)));
    if c.args.is_empty() {
        out.push_str("        args: &[],\n");
    } else {
        out.push_str("        args: &[\n");
        for a in &c.args {
            out.push_str("            ArgSpec {\n");
            out.push_str(&format!("                flag: {},\n", rust_str(&a.flag)));
            out.push_str(&format!("                arg: {},\n", rust_str(&a.arg)));
            out.push_str(&format!("                kind: {},\n", a.kind.rust_text()));
            out.push_str(&format!("                required: {},\n", a.required));
            match &a.default {
                Some(d) => out.push_str(&format!(
                    "                default: Some({}),\n",
                    rust_str(d)
                )),
                None => out.push_str("                default: None,\n"),
            }
            out.push_str(&format!("                help: {},\n", rust_str(&a.help)));
            out.push_str("            },\n");
        }
        out.push_str("        ],\n");
    }
    out.push_str("    },\n");
    out
}
