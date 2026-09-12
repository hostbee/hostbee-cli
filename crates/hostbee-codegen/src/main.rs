//! hostbee 命令面生成器（xtask 风格 bin）。
//!
//! 读取 vendored schema.graphql → 解析 → 领域分组 → 深度受限展开 →
//! 生成 `crates/hostbee/src/generated/{mod.rs,docs.rs}`（产物 commit 进仓库）。
//!
//! 运行：`cargo run -p hostbee-codegen`（默认输入 `vendor/backend/rustybee/schema.graphql`，
//! 可用 `--schema` 覆盖）。产物确定性排序，重跑幂等。
//!
//! fail-fast：schema 出现无法解析的类型引用、无法归属领域组的 root field、
//! 未映射的自定义标量时直接报错退出，绝不静默跳过。

mod domain;
mod emit;
mod expand;
mod schema;

use std::path::{Path, PathBuf};

use clap::{Arg, Command};
use sha2::{Digest, Sha256};

fn main() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let default_schema = workspace_root.join("vendor/backend/rustybee/schema.graphql");
    let out_dir = workspace_root.join("crates/hostbee/src/generated");

    let matches = Command::new("hostbee-codegen")
        .about("从 schema.graphql 生成 hostbee 全量命令面产物")
        .arg(
            Arg::new("schema")
                .long("schema")
                .value_name("PATH")
                .help("schema.graphql 路径（默认 vendored rustybee schema）"),
        )
        .get_matches();

    let schema_path = matches
        .get_one::<String>("schema")
        .map(PathBuf::from)
        .unwrap_or(default_schema);

    if let Err(err) = run(&schema_path, &out_dir) {
        eprintln!("生成失败: {err}");
        std::process::exit(1);
    }
}

fn run(schema_path: &Path, out_dir: &Path) -> Result<(), String> {
    let sdl = std::fs::read_to_string(schema_path)
        .map_err(|e| format!("读取 {} 失败: {e}", schema_path.display()))?;
    let schema_sha256 = hex(&Sha256::digest(sdl.as_bytes()));

    let model = schema::load(&sdl)?;
    let commands = emit::build_commands(&model)?;

    // 覆盖依据当前 schema 动态计算，不把历史勘察规模当作生成限制。
    let total = model.root_fields.len();
    for rf in &model.root_fields {
        domain::assign(&rf.name)?;
    }
    let expected: std::collections::BTreeSet<_> = model
        .root_fields
        .iter()
        .filter(|rf| !emit::SKIP_FIELDS.contains(&rf.name.as_str()))
        .map(|rf| rf.name.as_str())
        .collect();
    let actual: std::collections::BTreeSet<_> = commands.iter().map(|c| c.field.as_str()).collect();
    if actual != expected || actual.len() != commands.len() {
        return Err("生成命令未唯一完整覆盖当前 schema".to_owned());
    }

    // 产物 document 必须全部可被 query parser 解析（生成侧自查）。
    for c in &commands {
        for doc in &c.documents {
            async_graphql_parser::parse_query(doc)
                .map_err(|e| format!("{} 生成的 document 不可解析: {e}\n{doc}", c.field))?;
        }
    }

    let docs_rs = emit::emit_docs(&commands, &schema_sha256);
    let mod_rs = emit::emit_mod(&commands, &schema_sha256);
    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("创建产物目录 {} 失败: {e}", out_dir.display()))?;
    std::fs::write(out_dir.join("docs.rs"), docs_rs)
        .map_err(|e| format!("写 docs.rs 失败: {e}"))?;
    std::fs::write(out_dir.join("mod.rs"), mod_rs).map_err(|e| format!("写 mod.rs 失败: {e}"))?;

    // 人类可读摘要（生成器自身 stdout，不受 hostbee 输出契约约束）。
    let default_total: usize = commands
        .iter()
        .map(|c| c.documents[expand::DEFAULT_DEPTH].len())
        .sum();
    let default_max = commands
        .iter()
        .map(|c| c.documents[expand::DEFAULT_DEPTH].len())
        .max()
        .unwrap_or(0);
    println!(
        "生成 {} 个命令（覆盖 {total} root field，跳过 {:?}）",
        commands.len(),
        emit::SKIP_FIELDS
    );
    println!(
        "默认档 depth={}: 文档总 {} B，最大 {} B；schema sha256 {schema_sha256}",
        expand::DEFAULT_DEPTH,
        default_total,
        default_max
    );
    let out_dir = out_dir
        .canonicalize()
        .unwrap_or_else(|_| out_dir.to_path_buf());
    println!("产物目录: {}", out_dir.display());
    Ok(())
}

/// sha256 → hex（仅生成器内部用，避免引 hex crate）。
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::{kebab, scream};
    use crate::schema::SchemaModel;

    /// 测试用最小 schema：覆盖展开的环切/深度切/全标量类型三种终止形态。
    fn mini_schema() -> SchemaModel {
        schema::load(
            r#"
            schema { query: QueryRoot mutation: MutationRoot }
            scalar JSON
            type QueryRoot {
              vmPage(pageSize: Int!, pageNum: Int!, filter: Filter!): Paging!
              vmLoop: A!
              vmFlat: Flat!
              vmNoId: NoId!
              vmScalars(azIds: [Int!]!): [Int!]!
            }
            type MutationRoot {
              vmAct(input: ActInput!): String!
            }
            type Paging { nodes: [Node!]! totalNum: Int! }
            type Node { id: Int! name: String owner: A }
            type A { id: Int! b: B }
            type B { id: Int! a: A }
            type Flat { x: Int y: JSON z: Color }
            type NoId { name: String child: NoId }
            type NoScalar { peer: NoScalar }
            enum Color { RED GREEN }
            input Filter { hostname: String status: Color }
            input ActInput @oneOf { a: Filter b: Int }
            "#,
        )
        .expect("最小测试 schema 应可解析")
    }

    #[test]
    fn kebab_与_scream_转换() {
        assert_eq!(kebab("vmInstancesConnection"), "vm-instances-connection");
        assert_eq!(kebab("vmInit2"), "vm-init2");
        assert_eq!(kebab("backendS3Test"), "backend-s3-test");
        assert_eq!(scream("vmInstances"), "VM_INSTANCES");
        assert_eq!(scream("backendS3Test"), "BACKEND_S3_TEST");
    }

    #[test]
    fn 展开_环切_id_深度切_标量() {
        let schema = mini_schema();
        let expander = expand::Expander::new(&schema);
        // 环切：A→B→A 路径内重复 → { id }
        assert_eq!(
            expander.expand_object("A", &Default::default(), 8),
            "{ id b { id a { id } } }"
        );
        // 深度切：depth=0 时返回该类型的全部标量字段（Node 的 id、name；对象字段 owner 不进）
        assert_eq!(
            expander.expand_object("Node", &Default::default(), 0),
            "{ id name }"
        );
        // 全标量类型永远完整展开（Flat 含 JSON 标量与 enum）
        assert_eq!(
            expander.expand_object("Flat", &Default::default(), 0),
            "{ x y z }"
        );
        // 深度切无标量可取 → 回退环切形态（NoScalar 无 id → __typename）
        assert_eq!(
            expander.expand_object("NoScalar", &Default::default(), 0),
            "{ __typename }"
        );
        // 无 id 且有对象字段的类型：环切回退 __typename
        assert_eq!(
            expander.expand_object("NoId", &Default::default(), 8),
            "{ name child { __typename } }"
        );
    }

    #[test]
    fn build_commands_分页默认值与_oneof_hint() {
        let schema = mini_schema();
        let commands = emit::build_commands(&schema).unwrap();
        let page = commands.iter().find(|c| c.field == "vmPage").unwrap();
        // pageSize/pageNum 必填 → CLI 默认 25/1；filter 必填输入对象 → 默认 {}
        let by_flag = |flag: &str| page.args.iter().find(|a| a.flag == flag).unwrap();
        assert_eq!(by_flag("page-size").default.as_deref(), Some("25"));
        assert_eq!(by_flag("page-num").default.as_deref(), Some("1"));
        assert_eq!(by_flag("filter").default.as_deref(), Some("{}"));
        assert!(by_flag("filter").help.contains("JSON 透传"));
        // 标量列表 → IntList
        let scalars = commands.iter().find(|c| c.field == "vmScalars").unwrap();
        assert!(matches!(
            scalars.args.first().unwrap().kind,
            emit::ArgKindEmit::IntList
        ));
        // 标量返回 mutation：九档同一 document，无 selection set
        let act = commands.iter().find(|c| c.field == "vmAct").unwrap();
        assert!(!act.returns_object);
        assert_eq!(act.documents.len(), expand::DEPTH_COUNT);
        assert_eq!(
            act.documents[expand::DEFAULT_DEPTH],
            "mutation HostbeeVmAct($input: ActInput!) { vmAct(input: $input) }"
        );
        assert!(act.args[0].help.contains("oneOf"));
    }

    #[test]
    fn 未映射自定义标量_fail_fast() {
        let err = schema::load(
            r#"
            schema { query: Q mutation: M }
            scalar Weird
            type Q { vmF(x: Weird!): Int! }
            type M { m: Int! }
            "#,
        )
        .and_then(|m| emit::build_commands(&m));
        let err = err.err().expect("应 fail-fast");
        assert!(err.contains("Weird"), "错误应包含 Weird: {err}");
    }

    #[test]
    fn 未定义类型_fail_fast() {
        let err = schema::load(
            r#"
            schema { query: Q mutation: M }
            type Q { f: Ghost! }
            type M { m: Int! }
            "#,
        );
        assert!(err.err().expect("应 fail-fast").contains("Ghost"));
    }

    #[test]
    fn 参数与运行时_flag_撞名_fail_fast() {
        // 参数名 kebab 化后为 depth/fields/endpoint → 与 CLI 运行时 flag 撞名，生成期报错
        let err = schema::load(
            r#"
            schema { query: Q mutation: M }
            type Q { vmF(endpoint: String!): Int! }
            type M { vmM: Int! }
            "#,
        )
        .and_then(|m| emit::build_commands(&m));
        let err = err.err().expect("应 fail-fast");
        assert!(err.contains("撞名"), "错误应说明撞名: {err}");
        assert!(err.contains("endpoint"), "错误应包含 flag 名: {err}");
    }

    #[test]
    fn 参数_kebab_化后重复_fail_fast() {
        // `filter` 与 `Filter` kebab 化后同为 `filter` → 同命令内 flag 冲突
        let err = schema::load(
            r#"
            schema { query: Q mutation: M }
            input In { a: Int }
            type Q { vmF(filter: In!, Filter: In!): Int! }
            type M { vmM: Int! }
            "#,
        )
        .and_then(|m| emit::build_commands(&m));
        let err = err.err().expect("应 fail-fast");
        assert!(err.contains("冲突"), "错误应说明 flag 冲突: {err}");
    }
}

#[cfg(test)]
mod evolution_tests {
    #[test]
    fn 新增合法同域字段只需重跑且产物幂等() {
        let dir = tempfile::tempdir().unwrap();
        let schema = dir.path().join("schema.graphql");
        let out = dir.path().join("generated");
        for fields in ["vmOne: Int!", "vmOne: Int! vmTwo: String!"] {
            std::fs::write(
                &schema,
                format!("schema {{ query: Q mutation: M }} type Q {{ {fields} }} type M {{ vmAct: Int! }}"),
            )
            .unwrap();
            super::run(&schema, &out).unwrap();
        }
        let registry = std::fs::read_to_string(out.join("mod.rs")).unwrap();
        assert!(registry.contains("field: \"vmTwo\""));
        super::run(&schema, &out).unwrap();
        assert_eq!(
            registry,
            std::fs::read_to_string(out.join("mod.rs")).unwrap()
        );
    }
}
