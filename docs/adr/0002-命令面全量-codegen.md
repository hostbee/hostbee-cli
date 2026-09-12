# 命令面由 schema.graphql 全量 codegen 生成

后端 schema 庞大（3880 行）且持续演进，手写子命令无法维护。决定：从 `schema.graphql` 自动生成全部命令——Query/Mutation field 映射为按领域分组的子命令（vm、order、subscription、store、user、kyc、ticket、notice、plugin、admin…），参数映射为 flags，结果原样输出 JSON；schema 更新后重跑生成器。另保留手写的 `gql` 逃生门命令透传任意 GraphQL document。agent 因此不需要拼 GraphQL 字符串，省 token 且无字段名试错。

**Considered Options**: 手写高频命令（无法全覆盖，且 schema 变更即腐烂）；纯 `gql` 透传（agent 每次都要拼 document，效率低）。均否决。