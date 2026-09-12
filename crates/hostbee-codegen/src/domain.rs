//! root field → 领域组分组规则（spec-notes/schema-survey.md §1 的机器化实现）。
//!
//! 规则优先级：**显式归属表**（模糊归属与例外）→ **前缀规则链** → 无匹配即
//! fail-fast（schema 新增 field 须补一条规则，命令面不静默缺命令）。
//! 领域顺序固定为 [`DOMAIN_ORDER`]（help 列表与产物排序都用它）。

/// 领域组固定顺序（与 schema-survey.md §2 目录一致）。
pub const DOMAIN_ORDER: &[&str] = &[
    "auth",
    "user",
    "wallet",
    "vm",
    "infra",
    "store",
    "order",
    "subscription",
    "payment",
    "kyc",
    "ticket",
    "notice",
    "plugin",
    "task",
    "sms",
    "settings",
    "accesslog",
    "admin",
];

/// 模糊归属与例外：显式逐条指定（survey §1.2「模糊归属清单」+ auth 收编 + wallet 域）。
/// 排序无关；产物测试断言全部分组均接线，不锁定历史规模。
const OVERRIDES: &[(&str, &str)] = &[
    // ---- auth 收编：token/凭证语义字段，即使带 user 前缀也归 auth ----
    ("userAuth", "auth"),
    ("login", "auth"),
    ("loginWithMagicLink", "auth"),
    ("register", "auth"),
    ("refresh", "auth"),
    ("createPasskeyRegistrationOptions", "auth"),
    ("verifyPasskeyRegistration", "auth"),
    ("createPasskeyAuthenticationOptions", "auth"),
    ("verifyPasskeyAuthentication", "auth"),
    ("signTotp", "auth"),
    ("verifyTotp", "auth"),
    ("logoutAllDevices", "auth"),
    ("logout", "auth"),
    ("listRefreshTokens", "auth"),
    ("revokeRefreshToken", "auth"),
    ("sendPasswordResetEmail", "auth"),
    ("sendMagicLink", "auth"),
    ("sendVerificationCode", "auth"),
    ("verifyVerificationCode", "auth"),
    ("resetPassword", "auth"),
    ("updateUserInfo", "auth"),
    ("sendEmailVerification", "auth"),
    ("verifyEmail", "auth"),
    ("sendPhoneVerification", "auth"),
    ("verifyPhone", "auth"),
    ("removePasskey", "auth"),
    ("removeTotp", "auth"),
    ("verifyCaptcha", "auth"),
    ("generateCaptcha", "auth"),
    ("verifyAndUpdateEmail", "auth"),
    ("verifyAndUpdatePhone", "auth"),
    // ---- wallet 域：余额/流水/奖励（survey §1 规则 3）----
    ("userClaimablesPaging", "wallet"),
    ("allClaimables", "wallet"),
    ("userWallet", "wallet"),
    ("userTransactionList", "wallet"),
    ("claimReward", "wallet"),
    ("userBalanceAlter", "wallet"),
    ("adjustBalance", "wallet"),
    ("distributeReward", "wallet"),
    // ---- admin 杂项域 ----
    ("dayOverview", "admin"),
    ("backendVersion", "admin"),
    ("dangerRawQuery", "admin"),
    ("userMotd", "admin"),
    // ---- ticket：count 靠 filter 类型归属（survey §1.2）----
    ("count", "ticket"),
    // ---- vm：vnc/serial 控制台准备查询（survey §2 vm 组）----
    ("vncPrepare", "vm"),
    ("serialPrepare", "vm"),
    ("updateVmInstance", "vm"),
    // ---- infra：动词开头的管理面 mutation + 网络数据面运维 ----
    ("createRegion", "infra"),
    ("upsertRegion", "infra"),
    ("upsertAvailZone", "infra"),
    ("createHypervisor", "infra"),
    ("updateHypervisor", "infra"),
    ("upsertIspGateway", "infra"),
    ("createIspGatewayConnection", "infra"),
    ("updateIspGatewayConnection", "infra"),
    ("upsertStoragePool", "infra"),
    ("createIpPoolWithInputs", "infra"),
    ("addIpEntries", "infra"),
    ("changeIp", "infra"),
    ("upsertVmOsImage", "infra"),
    ("netifaceSync", "infra"),
    ("syncAllNetifaces", "infra"),
    // ---- store：快照与 ProductSpec 的动词开头 mutation ----
    ("previewStoreSnapshot", "store"),
    ("previewStoreSnapshots", "store"),
    ("currentStoreSnapshot", "store"),
    ("commitStoreSnapshot", "store"),
    ("rollbackStoreSnapshot", "store"),
    ("upsertProductSpecFamily", "store"),
    ("createProductSpec", "store"),
    ("updateProductSpec", "store"),
    // ---- order：payOrder 归订单（订单支付动作，非支付方式管理）----
    ("payOrder", "order"),
    ("createManualRenewalOrder", "order"),
    ("manualFulfillOrder", "order"),
    // ---- subscription：admin 变体与动词开头 mutation 归订阅域 ----
    ("updateSubscriptionNickname", "subscription"),
    // ---- payment：支付方式 CRUD 动词开头 ----
    ("createPaymentMethod", "payment"),
    ("updatePaymentMethod", "payment"),
    ("deletePaymentMethod", "payment"),
    ("togglePaymentMethodEnabled", "payment"),
    // ---- kyc：user 视图查询与动词开头 mutation ----
    ("userCurrentKycRecord", "kyc"),
    ("userLatestKycRecord", "kyc"),
    ("userApprovedKycRecords", "kyc"),
    ("markKycAsRevoked", "kyc"),
    ("markKycAsAdminExempted", "kyc"),
    ("createKycProvider", "kyc"),
    ("updateKycProvider", "kyc"),
    // ---- ticket：动词开头 mutation ----
    ("createTicket", "ticket"),
    ("replyTicketMessage", "ticket"),
    ("closeTicket", "ticket"),
    ("updateTicket", "ticket"),
    // ---- settings：站点设置自检（backend* 前缀规则覆盖 backend 侧）----
    ("siteGlobalSettings", "settings"),
    ("siteGlobalSettingsAlter", "settings"),
    // ---- accesslog ----
    ("exportAccessLogs", "accesslog"),
];

/// field 名 → 领域组。无匹配返回 Err（fail-fast）。
pub fn assign(field: &str) -> Result<&'static str, String> {
    if let Some((_, domain)) = OVERRIDES.iter().find(|(name, _)| *name == field) {
        return Ok(domain);
    }
    // 前缀规则链（survey §1 规则 1）。`categor` 兼容 category/categories 复数。
    let prefixes: &[(&str, &str)] = &[
        ("accessLog", "accesslog"),
        ("backend", "settings"),
        ("user", "user"),
        ("subscription", "subscription"),
        ("order", "order"),
        ("kyc", "kyc"),
        ("sms", "sms"),
        ("notice", "notice"),
        ("ticket", "ticket"),
        ("task", "task"),
        ("vm", "vm"),
        ("plugin", "plugin"),
        ("payment", "payment"),
        ("pay", "payment"),
        ("product", "store"),
        ("categor", "store"),
        ("store", "store"),
        ("region", "infra"),
        ("hypervisor", "infra"),
        ("avail", "infra"),
        ("gateway", "infra"),
        ("ip", "infra"),
        ("osImage", "infra"),
        ("storagePool", "infra"),
        ("netiface", "infra"),
    ];
    for (prefix, domain) in prefixes {
        if field.starts_with(prefix) {
            return Ok(domain);
        }
    }
    Err(format!(
        "root field {field} 无法归属任何领域组：请在 hostbee-codegen 的 domain.rs 增补规则\
         （OVERRIDES 或前缀链，参照 spec-notes/schema-survey.md §1）"
    ))
}
