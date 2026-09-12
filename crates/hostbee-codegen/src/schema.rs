//! schema.graphql → codegen 内部领域模型。
//!
//! 用 async-graphql-parser（与后端同家族，选型见 spec-notes/codegen-design.md §2.1）
//! 解析 SDL，提取 codegen 关心的最小模型：root field、对象类型、enum、input、标量。
//! 解析后做 **fail-fast 校验**：任何引用不到的类型（参数或返回值）直接报错，
//! 而不是静默跳过——schema 更新后重跑生成器即暴露问题，命令面不会腐烂。

use std::collections::{BTreeMap, BTreeSet};

use async_graphql_parser::types::{BaseType, Type, TypeKind, TypeSystemDefinition};

/// GraphQL 内置标量。
pub const BUILTIN_SCALARS: &[&str] = &["Int", "Float", "String", "Boolean", "ID"];

/// 解析后的类型引用，如 `VmQueryFilter!`、`[OrderQueryFilter!]!`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeRef {
    pub base: BaseRef,
    /// 外层是否可空（`!` 非空）。
    pub nullable: bool,
}

/// 类型引用的基座：具名类型或列表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseRef {
    Named(String),
    List(Box<TypeRef>),
}

impl TypeRef {
    fn from_parser(ty: &Type) -> Self {
        let base = match &ty.base {
            BaseType::Named(name) => BaseRef::Named(name.as_str().to_owned()),
            BaseType::List(inner) => BaseRef::List(Box::new(TypeRef::from_parser(inner))),
        };
        TypeRef {
            base,
            nullable: ty.nullable,
        }
    }

    /// 列表包装全部剥掉后的具名类型名。
    pub fn named(&self) -> &str {
        match &self.base {
            BaseRef::Named(name) => name,
            BaseRef::List(inner) => inner.named(),
        }
    }

    /// 最外层是否列表。
    pub fn is_list(&self) -> bool {
        matches!(self.base, BaseRef::List(_))
    }

    /// 最外层是否非空（GraphQL 必填位置）。
    pub fn required(&self) -> bool {
        !self.nullable
    }

    /// 渲染回 GraphQL 文本（变量声明用），如 `[OrderQueryFilter!]!`。
    pub fn render(&self) -> String {
        let bang = if self.nullable { "" } else { "!" };
        match &self.base {
            BaseRef::Named(name) => format!("{name}{bang}"),
            BaseRef::List(inner) => format!("[{}]{}", inner.render(), bang),
        }
    }
}

/// 对象类型的字段（本 schema 已断言：非 root 对象字段不带参数）。
pub struct FieldModel {
    pub name: String,
    pub ret: TypeRef,
}

/// root field 的参数 / input object 的成员。
#[derive(Clone)]
pub struct ArgModel {
    pub name: String,
    pub ty: TypeRef,
    pub doc: String,
}

/// root field：Query/Mutation 下的一个 field，映射为一个子命令。
pub struct RootFieldModel {
    pub name: String,
    pub doc: String,
    pub args: Vec<ArgModel>,
    pub ret: TypeRef,
    /// MutationRoot 下的 field 为 true。
    pub mutation: bool,
}

/// schema 的最小模型。
pub struct SchemaModel {
    /// root field（Query 原序在前，Mutation 原序在后）。
    pub root_fields: Vec<RootFieldModel>,
    /// 对象类型名 → 字段（schema 原序）；root 类型不在此表。
    pub objects: BTreeMap<String, Vec<FieldModel>>,
    /// enum 名 → 合法值。
    pub enums: BTreeMap<String, Vec<String>>,
    /// input object 名 → 成员。
    pub inputs: BTreeMap<String, Vec<ArgModel>>,
    /// 自定义标量名。
    pub scalars: BTreeSet<String>,
    /// 带 `@oneOf` 的 input 名。
    pub one_of: BTreeSet<String>,
}

/// 从 SDL 文本构建模型并做 fail-fast 校验。
pub fn load(sdl: &str) -> Result<SchemaModel, String> {
    let doc = async_graphql_parser::parse_schema(sdl)
        .map_err(|e| format!("schema.graphql 不是合法 SDL: {e}"))?;

    // 第一遍：找 schema 定义（query/mutation root 类型名）。
    let mut query_root = None;
    let mut mutation_root = None;
    for def in &doc.definitions {
        if let TypeSystemDefinition::Schema(s) = def {
            let s = &s.node;
            if s.extend {
                continue;
            }
            query_root = s.query.as_ref().map(|n| n.node.as_str().to_owned());
            mutation_root = s.mutation.as_ref().map(|n| n.node.as_str().to_owned());
        }
    }
    let query_root = query_root.ok_or("schema 缺少 `schema { query: ... }` 声明")?;
    let mutation_root = mutation_root.ok_or("schema 缺少 `schema { mutation: ... }` 声明")?;

    // 第二遍：收集全部类型定义（参数一并收集，root 归属稍后判定）。
    struct RawField {
        name: String,
        doc: String,
        args: Vec<ArgModel>,
        ret: TypeRef,
    }
    let mut raw_objects: BTreeMap<String, Vec<RawField>> = BTreeMap::new();
    let mut enums: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut inputs: BTreeMap<String, Vec<ArgModel>> = BTreeMap::new();
    let mut scalars: BTreeSet<String> = BTreeSet::new();
    let mut one_of: BTreeSet<String> = BTreeSet::new();

    for def in &doc.definitions {
        let TypeSystemDefinition::Type(t) = def else {
            continue;
        };
        let td = &t.node;
        if td.extend {
            return Err(format!("不支持 schema 扩展定义：type {}", td.name.node));
        }
        let name = td.name.node.as_str().to_owned();
        if td
            .directives
            .iter()
            .any(|d| d.node.name.node.as_str() == "oneOf")
        {
            one_of.insert(name.clone());
        }
        let args_of = |fd: &async_graphql_parser::types::FieldDefinition| -> Vec<ArgModel> {
            fd.arguments
                .iter()
                .map(|a| {
                    let ad = &a.node;
                    ArgModel {
                        name: ad.name.node.as_str().to_owned(),
                        ty: TypeRef::from_parser(&ad.ty.node),
                        doc: doc_text(&ad.description),
                    }
                })
                .collect()
        };
        match &td.kind {
            TypeKind::Object(o) => {
                let fields = o
                    .fields
                    .iter()
                    .map(|f| {
                        let fd = &f.node;
                        RawField {
                            name: fd.name.node.as_str().to_owned(),
                            doc: doc_text(&fd.description),
                            args: args_of(fd),
                            ret: TypeRef::from_parser(&fd.ty.node),
                        }
                    })
                    .collect();
                raw_objects.insert(name, fields);
            }
            TypeKind::InputObject(io) => {
                let fields = io
                    .fields
                    .iter()
                    .map(|f| {
                        let fd = &f.node;
                        ArgModel {
                            name: fd.name.node.as_str().to_owned(),
                            ty: TypeRef::from_parser(&fd.ty.node),
                            doc: doc_text(&fd.description),
                        }
                    })
                    .collect();
                inputs.insert(name, fields);
            }
            TypeKind::Enum(e) => {
                let values = e
                    .values
                    .iter()
                    .map(|v| v.node.value.node.as_str().to_owned())
                    .collect();
                enums.insert(name, values);
            }
            TypeKind::Scalar => {
                scalars.insert(name);
            }
            TypeKind::Interface(_) | TypeKind::Union(_) => {
                return Err(format!(
                    "不支持 interface/union 类型：{name}（本 schema 历史上没有）"
                ));
            }
        }
    }

    // 第三遍：按 root 归属拆分 root field 与普通对象字段。
    // root field 顺序固定为 Query 原序在前、Mutation 原序在后（不受 BTreeMap 字母序影响）。
    let mut root_fields = Vec::new();
    let roots = [(query_root.as_str(), false), (mutation_root.as_str(), true)];
    for (root_name, mutation) in roots {
        let Some(fields) = raw_objects.get(root_name) else {
            return Err(format!("root 类型 {root_name} 没有对应的 type 定义"));
        };
        for f in fields {
            root_fields.push(RootFieldModel {
                name: f.name.clone(),
                doc: f.doc.clone(),
                args: f.args.clone(),
                ret: f.ret.clone(),
                mutation,
            });
        }
    }
    let mut objects: BTreeMap<String, Vec<FieldModel>> = BTreeMap::new();
    for (type_name, fields) in raw_objects {
        if roots.iter().any(|(root_name, _)| *root_name == type_name) {
            continue;
        }
        if let Some(f) = fields.iter().find(|f| !f.args.is_empty()) {
            return Err(format!(
                "不支持对象字段带参数：{type_name}.{}（仅 root field 允许参数）",
                f.name
            ));
        }
        objects.insert(
            type_name,
            fields
                .into_iter()
                .map(|f| FieldModel {
                    name: f.name,
                    ret: f.ret,
                })
                .collect(),
        );
    }

    let model = SchemaModel {
        root_fields,
        objects,
        enums,
        inputs,
        scalars,
        one_of,
    };
    validate(&model)?;
    Ok(model)
}

/// docstring：多行压成一行（去首尾空白）。
fn doc_text(desc: &Option<async_graphql_parser::Positioned<String>>) -> String {
    desc.as_ref()
        .map(|d| {
            d.node
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// fail-fast 校验：所有类型引用必须可解析到已知定义。
fn validate(model: &SchemaModel) -> Result<(), String> {
    // 对象/enum/标量（含内置）都算已知具名类型。
    let known = |name: &str| {
        model.objects.contains_key(name)
            || model.enums.contains_key(name)
            || model.scalars.contains(name)
            || BUILTIN_SCALARS.contains(&name)
    };
    if model.root_fields.is_empty() {
        return Err("schema 没有 root field".to_owned());
    }
    for rf in &model.root_fields {
        if !known(rf.ret.named()) {
            return Err(format!(
                "root field {} 的返回类型 {} 未定义",
                rf.name,
                rf.ret.named()
            ));
        }
        for arg in &rf.args {
            let name = arg.ty.named();
            if model.inputs.contains_key(name)
                || model.enums.contains_key(name)
                || model.scalars.contains(name)
                || BUILTIN_SCALARS.contains(&name)
            {
                continue;
            }
            return Err(format!(
                "root field {} 的参数 {} 的类型 {name} 未定义",
                rf.name, arg.name
            ));
        }
    }
    for (type_name, fields) in &model.objects {
        for f in fields {
            if !known(f.ret.named()) {
                return Err(format!(
                    "对象 {type_name} 的字段 {} 引用了未定义类型 {}",
                    f.name,
                    f.ret.named()
                ));
            }
        }
    }
    for (type_name, fields) in &model.inputs {
        for f in fields {
            // input 成员可引用：其他 input、enum、标量（含内置）。
            if model.inputs.contains_key(f.ty.named())
                || model.enums.contains_key(f.ty.named())
                || model.scalars.contains(f.ty.named())
                || BUILTIN_SCALARS.contains(&f.ty.named())
            {
                continue;
            }
            return Err(format!(
                "input {type_name} 的成员 {} 引用了未定义类型 {}",
                f.name,
                f.ty.named()
            ));
        }
    }
    Ok(())
}
