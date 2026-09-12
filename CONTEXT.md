# hostbee-cli

rustybee 后端（GraphQL）的 CLI 客户端，唯一消费者是持有 admin 账号、长期自主运行的 agent。本文档是术语表，不含实现细节。

## Language

### 基础设施

**Region**:
地理区域，是 AvailZone 的分组容器。
_Avoid_: 地区、机房

**AvailZone**:
Region 内的可用区，关联 Hypervisor。
_Avoid_: AZ、zone

**Hypervisor**:
一台宿主节点（PVE），通过 peerId 标识，承载 VmInstance。
_Avoid_: 节点、node、宿主机

**VmInstance**:
用户的一台虚拟机实例，有电源状态、OS 镜像与网络接口。
_Avoid_: VM、虚拟机、实例

**VmOsImage**:
VmInstance 可用的操作系统镜像，按 Region 划分。

**IpPool / IpEntry**:
IP 地址池与其中的单个 IP 条目，分配给 VmInstance 的网络接口。

**Netiface**:
VmInstance 的网络接口，IP 分配的载体。
_Avoid_: 网卡

**IspGateway**:
连接 ISP 的出口网关，VmInstance 经它对外通信。

**StoragePool**:
存储资源池。

### 商务

**Category**:
商品类目，ProductGroup 的顶层分组。

**ProductGroup**:
一组规格相近的 Product 的分组。
_Avoid_: 套餐组

**Product**:
一个可售商品，属于某个 ProductGroup。

**ProductSpec / ProductSpecFamily**:
商品的具体规格及规格族，决定 VmInstance 的资源形态。

**Order**:
一次购买请求，支付后产生 Subscription。
_Avoid_: 订单请求、purchase

**Subscription**:
周期计费的订购关系，有 cycleLength、cyclePrice、dueDate，续费与到期均围绕它。
_Avoid_: 订阅服务、billing subscription 与 GraphQL subscription 无关，注意区分

**Wallet**:
用户的余额钱包，含 Transaction 流水。
_Avoid_: 账户余额、balance

**Transaction**:
Wallet 的一次资金变动记录。

**Reward**:
可领取的奖励，用户通过 claim 领取。

### 账户与运营

**User**:
平台的账户，admin 账号是一种持有全部权限的 User。
_Avoid_: 客户、customer

**Contact**:
登录用的联系方式（邮箱或手机号），login 的入参。

**AccessToken / RefreshToken**:
登录后签发的凭证对，refresh 会轮换两者。

**KYC**:
用户实名认证流程与记录。

**Ticket**:
用户工单。

**Notice**:
站内通知公告。

**Plugin**:
后端插件（如 pvebee），可通过 plugin mutation 与公共路由扩展后端能力。

### CLI 侧

**gql 逃生门**:
`hostbee gql` 命令，直接透传任意 GraphQL document，覆盖 codegen 尚未生成命令的窗口期。

**PageInfo**:
分页查询结果中附带的翻页信息（cursor / hasNextPage），CLI 默认单页返回并输出它，由 agent 自行决定翻页。