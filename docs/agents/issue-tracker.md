# Issue tracker: GitHub

本仓库的 issues 和 specs 存放在 GitHub Issues 中,所有操作使用 `gh` CLI。

## 约定

- **创建 issue**:`gh issue create --title "..." --body "..."`,多行 body 用 heredoc。
- **读取 issue**:`gh issue view <number> --comments`,用 `jq` 过滤评论并附带 labels。
- **列出 issues**:`gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'`,按需加 `--label` 和 `--state` 过滤。
- **评论**:`gh issue comment <number> --body "..."`
- **加/移除 label**:`gh issue edit <number> --add-label "..."` / `--remove-label "..."`
- **关闭**:`gh issue close <number> --comment "..."`

仓库从 `git remote -v` 推断;在 clone 内运行时 `gh` 自动识别。

## Pull requests 作为 triage 面

**PRs as a request surface: no.** _(如本仓库将外部 PR 视为 feature request,改为 `yes`;`/triage` 读取此标志。)_

设为 `yes` 时,PR 走与 issue 相同的 labels 和状态,使用对应的 `gh pr` 命令:

- **读取 PR**:`gh pr view <number> --comments`,diff 用 `gh pr diff <number>`。
- **列出待 triage 的外部 PR**:`gh pr list --state open --json number,title,body,labels,author,authorAssociation,comments`,只保留 `authorAssociation` 为 `CONTRIBUTOR`、`FIRST_TIME_CONTRIBUTOR` 或 `NONE` 的(去掉 `OWNER`/`MEMBER`/`COLLABORATOR`)。
- **评论/加 label/关闭**:`gh pr comment`、`gh pr edit --add-label`/`--remove-label`、`gh pr close`。

GitHub 的 issue 和 PR 共用同一编号空间,裸 `#42` 可能是任一种:先 `gh pr view 42`,失败再 `gh issue view 42`。

## 当 skill 说"publish to the issue tracker"

创建一个 GitHub issue。

## 当 skill 说"fetch the relevant ticket"

运行 `gh issue view <number> --comments`。

## Wayfinder 操作

供 `/wayfinder` 使用。**地图(map)** 是一个 issue,**子 ticket** 是其 child issues。

- **地图**:单个 issue,label 为 `wayfinder:map`,body 含 Notes / Decisions-so-far / Fog。`gh issue create --label wayfinder:map`。
- **子 ticket**:通过 GitHub sub-issues(`gh api` sub-issues endpoint)链接到地图。sub-issues 不可用时,把子项加进地图 body 的 task list,子项 body 顶部写 `Part of #<map>`。Labels:`wayfinder:<type>`(`research`/`prototype`/`grilling`/`task`)。被认领后 assign 给驱动的开发。
- **Blocking**:GitHub 原生 issue dependencies。加边:`gh api --method POST repos/<owner>/<repo>/issues/<child>/dependencies/blocked_by -F issue_id=<blocker-db-id>`,`<blocker-db-id>` 是 blocker 的数值 **database id**(`gh api repos/<owner>/<repo>/issues/<n> --jq .id`,不是 `#number` 也不是 `node_id`)。GitHub 提供 `issue_dependencies_summary.blocked_by`(仅计 open blocker,即实时门禁)。dependencies 不可用时,在子项 body 顶部写 `Blocked by: #<n>, #<n>`。所有 blocker 关闭后子项才算解锁。
- **Frontier 查询**:列出地图的 open children(`gh issue list --state open`,限定地图的 sub-issues / task list),去掉有 open blocker 的(`issue_dependencies_summary.blocked_by > 0`,或 `Blocked by` 行中有 open issue)或有 assignee 的;按地图顺序取第一个。
- **认领**:`gh issue edit <n> --add-assignee @me`,这是会话的第一个写操作。
- **解决**:`gh issue comment <n> --body "<answer>"`,然后 `gh issue close <n>`,最后把 context 指针(gist + 链接)追加到地图的 Decisions-so-far。