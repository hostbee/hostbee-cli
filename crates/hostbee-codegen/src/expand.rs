//! 深度受限 selection set 展开（spec-notes/codegen-design.md §1.1，算法与
//! spec-notes/tools/schema-tools/measure2.py 逐行对齐，已在 live 后端实测）。
//!
//! 规则要点：
//! 1. depth 语义 = **对象嵌套层数**：标量/enum 不消耗深度；
//!    字段全是标量的类型（PageInfo、MetadataJson 等）永远完整展开；
//! 2. 环切：路径内重复出现的类型 → `{ id }`（无 `id` 字段的类型 → `{ __typename }`）；
//! 3. 深度切：到 depth 上限时返回该类型的全部标量字段（比 `{ id }` 信息量大），
//!    无标量字段则回退环切形态。

use std::collections::BTreeSet;

use crate::schema::SchemaModel;

/// 展开深度档位：0..=MAX_DEPTH（`--depth` flag 的取值范围）。
pub const MAX_DEPTH: usize = 8;
/// 深度档位总数（含 0）。
pub const DEPTH_COUNT: usize = MAX_DEPTH + 1;
/// 默认档（`--depth` 默认值，产物 `documents[DEFAULT_DEPTH]` 即默认 document）。
pub const DEFAULT_DEPTH: usize = 3;

/// 在 schema 模型上做深度受限展开。
pub struct Expander<'a> {
    schema: &'a SchemaModel,
}

impl<'a> Expander<'a> {
    pub fn new(schema: &'a SchemaModel) -> Self {
        Expander { schema }
    }

    /// 具名类型是否对象类型（展开目标）。
    fn is_object(&self, name: &str) -> bool {
        self.schema.objects.contains_key(name)
    }

    /// 该类型的全部标量/enum 字段名（深度切的产物；None = 无标量字段）。
    fn scalar_fields(&self, name: &str) -> Option<Vec<&str>> {
        let fields = self.schema.objects.get(name)?;
        let scalars: Vec<&str> = fields
            .iter()
            .filter(|f| !self.is_object(f.ret.named()))
            .map(|f| f.name.as_str())
            .collect();
        (!scalars.is_empty()).then_some(scalars)
    }

    fn has_id(&self, name: &str) -> bool {
        self.schema
            .objects
            .get(name)
            .is_some_and(|fields| fields.iter().any(|f| f.name == "id"))
    }

    /// 环切终止形态：路径内重复 → `{ id }`（无 `id` 字段的类型 → `{ __typename }`）。
    fn cycle_cut(&self, name: &str) -> String {
        if self.has_id(name) {
            "{ id }".to_owned()
        } else {
            "{ __typename }".to_owned()
        }
    }

    /// 深度切终止形态：该类型的全部标量字段（比 `{ id }` 信息量大）；
    /// 无标量字段则回退环切形态。
    fn depth_cut(&self, name: &str) -> String {
        match self.scalar_fields(name) {
            Some(scalars) => format!("{{ {} }}", scalars.join(" ")),
            None => self.cycle_cut(name),
        }
    }

    /// 展开一个对象类型为 selection set（含外层花括号）；深度按对象嵌套递减。
    pub fn expand_object(&self, name: &str, seen: &BTreeSet<String>, depth: usize) -> String {
        if seen.contains(name) {
            return self.cycle_cut(name);
        }
        if depth == 0 {
            return self.depth_cut(name);
        }
        let mut next = seen.clone();
        next.insert(name.to_owned());
        let fields = &self.schema.objects[name];
        let parts: Vec<String> = fields
            .iter()
            .map(|f| {
                let ret = f.ret.named();
                if !self.is_object(ret) {
                    f.name.clone()
                } else {
                    format!("{} {}", f.name, self.expand_object(ret, &next, depth - 1))
                }
            })
            .collect();
        format!("{{ {} }}", parts.join(" "))
    }

    /// root field 的 selection set（含外层花括号）；返回值为标量时返回 None。
    pub fn root_selection(&self, ret_named: &str, depth: usize) -> Option<String> {
        if self.is_object(ret_named) {
            Some(self.expand_object(ret_named, &BTreeSet::new(), depth))
        } else {
            None
        }
    }
}
