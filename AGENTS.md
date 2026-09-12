# Repository Rules

## Repository Instructions

- 所有文档和 Markdown 文件必须主要使用中文编写，中文习惯上不翻译的术语才保留。

## Commit conventions

- 格式：`<type>[optional scope]: <description>`
- `<type>` 和 `[optional scope]` 使用英文。
- `<description>` 使用简体中文，中文习惯上不翻译的英文技术术语保持英文。
- type 可选值：`feat`、`fix`、`chore`、`docs`、`refactor`、`perf`、`test`、`ci`、`build`、`style`、`revert`。

## PR conventions

- PR 标题和描述使用简体中文，中文习惯上不翻译的英文技术术语保持英文。
- PR 标题格式：`<type>[optional scope]: <description>`，同 Commit 规范，中文习惯上不翻译的术语才

## Agent skills

### Issue tracker

Issues 存放在 GitHub Issues（hostbee/hostbee-cli），用 `gh` CLI 读写。见 `docs/agents/issue-tracker.md`。

### Triage labels

使用五个默认 triage 标签（needs-triage、needs-info、ready-for-agent、ready-for-human、wontfix）。见 `docs/agents/triage-labels.md`。

### Domain docs

Single-context 布局：根目录 `CONTEXT.md` + `docs/adr/`。见 `docs/agents/domain.md`。
