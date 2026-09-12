# Domain Docs

工程 skills 探索代码库时应如何消费本仓库的领域文档。

## 探索前先读这些

- 仓库根目录的 **`CONTEXT.md`**;若存在根目录 **`CONTEXT-MAP.md`**,则按其指向读取与主题相关的每个 `CONTEXT.md`。
- **`docs/adr/`**:阅读与你即将工作的区域相关的 ADR。multi-context 仓库还需检查 `src/<context>/docs/adr/` 的 context 级决策。

这些文件不存在时**静默继续**。不要提示缺失,也不要建议预先创建。`/domain-modeling` skill(经由 `/grill-with-docs` 和 `/improve-codebase-architecture` 触达)会在术语或决策真正被确定时惰性创建它们。

## 文件结构

Single-context 仓库(大多数仓库):

```
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0001-event-sourced-orders.md
│   └── 0002-postgres-for-write-model.md
└── src/
```

Multi-context 仓库(根目录存在 `CONTEXT-MAP.md`):

```
/
├── CONTEXT-MAP.md
├── docs/adr/                          ← 系统级决策
└── src/
    ├── ordering/
    │   ├── CONTEXT.md
    │   └── docs/adr/                  ← context 专属决策
    └── billing/
        ├── CONTEXT.md
        └── docs/adr/
```

## 使用 glossary 的词汇

当你的输出提及领域概念(issue 标题、重构提案、假设、测试名)时,使用 `CONTEXT.md` 定义的术语。不要漂移到 glossary 明确避免的同义词。

需要的概念不在 glossary 中时,这是信号:要么你在发明项目不用的语言(重新考虑),要么存在真实缺口(记下来交给 `/domain-modeling`)。

## 标记 ADR 冲突

若你的输出与既有 ADR 矛盾,显式指出而非静默覆盖:

> _与 ADR-0007(event-sourced orders)矛盾,但值得重新审视,因为……_