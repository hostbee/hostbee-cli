//! codegen 产物断言（ticket #4 验收：重跑可再生成、产物有 unit 断言）。
//!
//! 断言对象是「生成器产物 ↔ 当前 vendored schema」的一致性：
//! - schema sha256 漂移标记（schema 变了请重跑生成器）；
//! - 结构断言：全部 root field（除 login/refresh）都有命令、领域分组与
//!   schema-survey.md §2 目录一致、vm 组 12 命令齐全；
//! - document 合法性：208×9 全量 `parse_query` 可解析、`--fields` 拼接路径同样可解析；
//! - 快照断言：tracer（vmInstances）的默认 document 逐字节锁定；
//! - 尺寸预算：默认档文档总量不超基线（防展开失控）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hostbee::generated::docs;
use hostbee::generated::{FIELDS, SCHEMA_SHA256};
use sha2::{Digest, Sha256};

/// vendored schema 路径（相对本 crate 根；与生成器默认输入一致）。
fn schema_sdl() -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/backend/rustybee/schema.graphql");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 vendored schema 失败（submodule 未初始化？）: {e}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 解析 schema 得到全部 root field 名（Query 原序 + Mutation 原序）。
fn root_field_names(sdl: &str) -> Vec<String> {
    use async_graphql_parser::types::{TypeKind, TypeSystemDefinition};
    let doc = async_graphql_parser::parse_schema(sdl).expect("vendored schema 应可解析");
    let mut roots = Vec::new();
    for def in &doc.definitions {
        if let TypeSystemDefinition::Schema(s) = def {
            roots.push(s.node.query.as_ref().map(|n| n.node.as_str().to_owned()));
            roots.push(s.node.mutation.as_ref().map(|n| n.node.as_str().to_owned()));
        }
    }
    let mut names = Vec::new();
    for def in &doc.definitions {
        let TypeSystemDefinition::Type(t) = def else {
            continue;
        };
        let td = &t.node;
        let TypeKind::Object(o) = &td.kind else {
            continue;
        };
        if roots.contains(&Some(td.name.node.as_str().to_owned())) {
            names.extend(
                o.fields
                    .iter()
                    .map(|f| f.node.name.node.as_str().to_owned()),
            );
        }
    }
    names
}

// ========== 漂移标记 ==========

#[test]
fn schema_sha256_漂移标记一致() {
    let sdl = schema_sdl();
    let actual = sha256_hex(sdl.as_bytes());
    assert_eq!(
        actual, SCHEMA_SHA256,
        "schema 已变更：请重跑 `cargo run -p hostbee-codegen` 并提交新产物"
    );
}

// ========== 结构断言 ==========

#[test]
fn 全部_root_field_都有命令_除_login_refresh() {
    let names = root_field_names(&schema_sdl());
    assert_eq!(names.len(), 210, "schema root field 总数应为 210");
    let covered: BTreeSet<&str> = FIELDS.iter().map(|f| f.field).collect();
    assert_eq!(covered.len(), FIELDS.len(), "注册表内 field 名不应重复");
    let expected: BTreeSet<&str> = names
        .iter()
        .map(|n| n.as_str())
        .filter(|n| !["login", "refresh"].contains(n))
        .collect();
    assert_eq!(covered, expected);
}

#[test]
fn 领域分组_与勘察目录一致() {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for f in FIELDS {
        *counts.entry(f.domain).or_default() += 1;
    }
    // schema-survey.md §2：auth 31 中 login/refresh 由 auth.rs 手写 → 剩 29。
    let expected = [
        ("auth", 29),
        ("user", 14),
        ("wallet", 8),
        ("vm", 12),
        ("infra", 27),
        ("store", 25),
        ("order", 9),
        ("subscription", 13),
        ("payment", 7),
        ("kyc", 11),
        ("ticket", 7),
        ("notice", 4),
        ("plugin", 5),
        ("task", 5),
        ("sms", 12),
        ("settings", 13),
        ("accesslog", 3),
        ("admin", 4),
    ];
    for (domain, count) in expected {
        assert_eq!(
            counts.get(domain),
            Some(&count),
            "领域 {domain} 计数不符（schema 变更后重跑生成器并核对 survey）"
        );
    }
    assert_eq!(FIELDS.len(), 208);
}

#[test]
fn vm_组_12_命令_齐全_且注册表含全_18_域() {
    let vm: Vec<&str> = FIELDS
        .iter()
        .filter(|f| f.domain == "vm")
        .map(|f| f.command)
        .collect();
    assert_eq!(
        vm,
        [
            "vm-instances",
            "vm-instance-by-subscription",
            "vm-by-subscription",
            "vnc-prepare",
            "serial-prepare",
            "vm-instance-search",
            "vm-instances-connection",
            "update-vm-instance",
            "vm-switch-egress",
            "vm-init",
            "vm-init2",
            "vm-power-action",
        ]
    );
    // ticket #5 翻牌前提：其余 17 域的数据已全量在注册表。
    let domains: BTreeSet<&str> = FIELDS.iter().map(|f| f.domain).collect();
    assert_eq!(domains.len(), 18, "注册表应覆盖全部 18 个领域组");
}

#[test]
fn 分页_flags_按风格生成() {
    // pageSize/pageNum 风格：必填但 CLI 给默认（无参可跑）。
    let instances = FIELDS.iter().find(|f| f.command == "vm-instances").unwrap();
    let arg = |name: &str| instances.args.iter().find(|a| a.arg == name).unwrap();
    assert_eq!(arg("pageSize").default, Some("25"));
    assert_eq!(arg("pageNum").default, Some("1"));
    assert_eq!(arg("filter").default, Some("{}"));
    assert!(arg("filter").required);
    // 游标风格：first 默认 25，after/before/last 可选无默认。
    let connection = FIELDS
        .iter()
        .find(|f| f.command == "vm-instances-connection")
        .unwrap();
    let arg = |name: &str| connection.args.iter().find(|a| a.arg == name).unwrap();
    assert_eq!(arg("first").default, Some("25"));
    assert_eq!(arg("after").default, None);
    assert_eq!(arg("before").default, None);
    assert_eq!(arg("last").default, None);
    assert!(!arg("after").required);
    // 普通必填标量：无默认，clap required。
    let init = FIELDS.iter().find(|f| f.command == "vm-init").unwrap();
    let arg = |name: &str| init.args.iter().find(|a| a.arg == name).unwrap();
    assert!(arg("vmId").required);
    assert_eq!(arg("vmId").default, None);
    assert_eq!(arg("rootPassword").default, None);
    // enum 参数带 schema 合法值。
    let power = FIELDS
        .iter()
        .find(|f| f.command == "vm-power-action")
        .unwrap();
    let action = power.args.iter().find(|a| a.arg == "action").unwrap();
    match &action.kind {
        hostbee::commands::ArgKind::Enum(values) => {
            assert_eq!(
                *values,
                [
                    "START",
                    "SHUTDOWN",
                    "REBOOT",
                    "FORCE_SHUTDOWN",
                    "FORCE_REBOOT"
                ]
            );
        }
        other => panic!("action 应为 Enum: {other:?}"),
    }
}

// ========== document 合法性 ==========

#[test]
fn 全部_document_九档均可被_parse_query_解析() {
    let mut parsed = 0;
    for spec in FIELDS {
        assert_eq!(spec.documents.len(), 9, "{} 应有 0..=8 九档", spec.field);
        for doc in spec.documents {
            async_graphql_parser::parse_query(doc)
                .unwrap_or_else(|e| panic!("{} 的 document 不可解析: {e}\n{doc}", spec.field));
            parsed += 1;
        }
    }
    assert_eq!(parsed, 208 * 9);
}

#[test]
fn fields_拼接路径_全部对象命令可解析() {
    for spec in FIELDS.iter().filter(|f| f.returns_object) {
        let doc = format!("{} {{ id }} }}", spec.prefix);
        async_graphql_parser::parse_query(&doc)
            .unwrap_or_else(|e| panic!("{} --fields 拼接不可解析: {e}\n{doc}", spec.field));
    }
}

#[test]
fn 标量返回命令_九档同文且无_selection_set() {
    let power = FIELDS
        .iter()
        .find(|f| f.command == "vm-power-action")
        .unwrap();
    assert!(!power.returns_object);
    for doc in power.documents {
        assert_eq!(*doc, power.documents[0]);
        // 仅 operation 一层花括号（标量返回：field 之后没有 selection set）。
        assert_eq!(
            doc.matches('{').count(),
            1,
            "标量返回不应有 selection set: {doc}"
        );
    }
}

// ========== 快照断言（tracer：vmInstances） ==========

#[test]
fn vm_instances_前缀与默认_document_快照() {
    assert_eq!(
        docs::VM_INSTANCES_PREFIX,
        "query HostbeeVmInstances($pageSize: Int!, $pageNum: Int!, $filter: VmQueryFilter!) \
         { vmInstances(pageSize: $pageSize, pageNum: $pageNum, filter: $filter)"
    );
    // 默认档（depth=3）完整 document 逐字节锁定：环切/深度切形态都在其中。
    assert_eq!(
        docs::VM_INSTANCES_DOCS[3],
        "query HostbeeVmInstances($pageSize: Int!, $pageNum: Int!, $filter: VmQueryFilter!) { vmInstances(pageSize: $pageSize, pageNum: $pageNum, filter: $filter) { nodes { id status subscription { id associatedUser { id email callingCode phoneNumber role status passwordChangeNeeded creditLimit allowTicket authProvider authProviderId availableKycChance emailVerifiedAt passwordResetAt emailVerificationTokenSentAt passwordResetTokenSentAt phoneVerifiedAt phoneVerificationTokenSentAt phoneVerificationToken createdAt updatedAt hasTotp hasPasskey } productId associatedProduct { name regionName availabilityZoneName productSpecName } associatedProviderId dueDate nextBillingDate nextBillingPrice cycleLength { unit length } cyclePrice allowTicket status metadata { pluginScope configurableOptions } notes createdAt updatedAt pendingOrderId subscriptionType isRenewable nickname } subscriptionId hypervisor { id name availZone { id name desc regionId enabled } availZoneId enabled ipAddress desc peerId allowSshPassword lastSeenAt vmInstances { id status subscriptionId hypervisorId osImageId rootPassword specCpu specMemory specBootDiskSize bootDiskStorageId running hostname } ispGatewayConnections { id ispGatewayId hypervisorId enabled netiface vlan ipAddr gatewayAddr pcapNetiface } } hypervisorId productSpecId osImage { id name humanName osType path desc region { id name enabled kycRequired } regionId vmInstances { id status subscriptionId hypervisorId osImageId rootPassword specCpu specMemory specBootDiskSize bootDiskStorageId running hostname } } osImageId rootPassword specCpu specMemory specBootDiskSize bootDiskStorage { id name pveStorage availZone { id name desc regionId enabled } availZoneId } bootDiskStorageId running netifaces { id vmInstance { id status subscriptionId hypervisorId osImageId rootPassword specCpu specMemory specBootDiskSize bootDiskStorageId running hostname } vmInstanceId bandwidthBillingType bandwidthBillingCycleUnit bandwidthBillingCycleLength bandwidthAmount ipAssignments { id ipString entryId allowIngress allowEgress egressFlag egressRate bandwidthMbpsIn bandwidthMbpsOut accumulatedTrafficInPackets accumulatedTrafficOutPackets accumulatedTrafficInBytes accumulatedTrafficOutBytes gatewayConnectionId } displayBandwidthMbpsIn displayBandwidthMbpsOut } hostname createdAt productSpecName userEmail } totalNum } }"
    );
    // 分页语义：输出含 nodes 与 totalNum；游标组含 pageInfo 与 totalCount。
    assert!(docs::VM_INSTANCES_DOCS[3].contains("{ nodes"));
    assert!(docs::VM_INSTANCES_DOCS[3].ends_with("totalNum } }"));
    assert!(docs::VM_INSTANCES_CONNECTION_DOCS[3].contains("pageInfo"));
    assert!(docs::VM_INSTANCES_CONNECTION_DOCS[3].contains("totalCount"));
}

#[test]
fn 环切快照_store_小环() {
    // Category→ProductGroup→Category：路径内重复处切到 { id }（codegen-design.md §1.2 实测样例）。
    assert!(docs::PREVIEW_STORE_SNAPSHOT_DOCS[3].contains("categories { id }"));
    assert!(docs::PREVIEW_STORE_SNAPSHOT_DOCS[3].contains("productGroups { id }"));
}

#[test]
fn 展开尺寸_不超基线() {
    // 实测基线：默认档总 131781 B、最大 3696 B（含变量声明前缀；codegen-design.md
    // §6 的 114448/3619 为纯 selection 口径）。超基线说明展开失控，先查生成器。
    let total: usize = FIELDS
        .iter()
        .map(|f| f.documents[hostbee::commands::DEFAULT_DEPTH].len())
        .sum();
    let max = FIELDS
        .iter()
        .map(|f| f.documents[hostbee::commands::DEFAULT_DEPTH].len())
        .max()
        .unwrap();
    assert!(total < 200_000, "默认档文档总量 {total} B 超基线");
    assert!(max < 8_000, "最大文档 {max} B 超基线");
}
