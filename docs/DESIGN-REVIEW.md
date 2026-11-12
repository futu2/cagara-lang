# Cagara 设计评审与改进建议

> 评审对象：Cagara 0.3.2（仓库 `af11149`，release 二进制 `target/release/cagara`）
> 评审时间：2026-11-06
> 方法：5 个并行评审维度（表面语法 / 类型系统 / 工具链 / SQL 后端 / 对标研究）
> ＋ 作者真实工程 `res/icmidle`（733 个 `.cagara`、6.52M 行、93,910 张表）
> ＋ 作者自记的 `res/icmidle/CAGARA-BUGS.md`
> 原始证据：`.cagara-review/01-surface.md`、`02-types.md`、`03-tooling.md`、
> `04-backend.md`、`05-comparison.md`（共 3,169 行，含每条的可复现命令与原始报错）

---

## 0. 结论

### 0.1 先说评价

Cagara 的**内核工艺明显高于"设计粗糙"这四个字**：类型化查询、显式可空、
相（phase）约束、行类型贯穿 pipeline、编译期拒绝未分组字段、Origin 保留、
诊断给可用列与修法、`fmt` 幂等且有形状校验、`doc` 从推断签名生成、
`cargo test --workspace` 全绿、性能文档还有 1M 表/增量编译的基准数据。
这些在同类"SQL 生成器"里是少见的。

真正的问题不在"实现糙"，而在**三个错位**：

1. **表面语言按 Haskell 审美设计，目标用户却更像 SQL / 分析用户。**
   subject-last、`&`、`$`、`>>>`、`<>` 表示拼接、`.<x`/`.>x`、符号简写、
   固定长 key 列表……这是一套自洽的函数式方言，但把学习成本转移给了
   "每天写报表的人"；同一件事还存在两种写法（具名 stage 与符号）。
2. **类型系统证明的东西，和 SQL 实际保证的东西之间有缝。**
   除法/取模被当成全函数（SQL 里是偏函数、还会溢出）、`toDecimal` 只是
   SQL 侧 CAST 而语言类型仍是 `float`、方言"能生成"被当成"能运行"、
   排序键被强制非空导致 `NULLS LAST` 机制空转。
3. **它是编译器，不是日常工具。**
   表结构靠外部 Python 脚本从 `tables.jsonl` 生成；没有项目清单/目录级
   `check`/机器可读输出/持久缓存/执行回路；LSP 只发布 root 文件诊断
   （`crates/cagara-lsp/src/analysis.rs:30`），真实工程里会出现
   "编辑器全绿、CI 红"；9.4 MB 的单模块让一次 CLI 编译 ≈1.35 s / 236 MB。

### 0.2 最危险的 6 个静默错误（不是"丑"，是"错数"）

| # | 问题 | 现状 |
|---|---|---|
| 1 | join 同名列静默"左胜" | 右表同名列**永久不可达**，无 warning，`--types` 也看不出 |
| 2 | `limit` 后接 `offset` | 恒返回 0 行；`docs/LEARN.md:696` 与 `CAGARA.md` §5.5 的官方示例本身就是错的 |
| 3 | nullable join 键 | 自然写法报 `kind mismatch`；唯一能跑的 `coalesce 0` 写法会**错配**真实 0 键行 |
| 4 | 未知类型名 | `amount = decimal` 静默变成自由类型变量；同一列可被三个定义读成 `int`/`string`/`float` 全部通过 |
| 5 | `/`、`%`、整数 `+` | 类型说 `int`，SQLite 实测除零得 `NULL`、`INT64_MAX+1` 溢出成 `REAL` |
| 6 | 未验证方言 | `--dialect oracle` 生成 `DATE_TRUNC(...)`（Oracle 无此函数）、`--dialect hive` 生成裸 `OFFSET`，CLI 零警告 |

### 0.3 建议的总方向

**保持内核（HM + 行类型 + 可空 + phase + arena），收敛表面（只留一套具名
stage 写法），补齐工作流（schema 来源、项目命令、执行回路），把所有静默
错误改成编译错误，再把 `sql` 后端从"生成字符串"升级成"带结构化错误和
方言分级的目标"。** 下面按"会错数 → 每天摩擦 → 表达力 → 工具链 → 架构"
排序，每条都给证据、改法与成本（S ≈ 小时级，M ≈ 数天，L ≈ 数周）。

---

## 1. 先划清：这些设计值得保留，不要为了"简洁"动它们

| 设计 | 为什么值得保留 |
|---|---|
| `&` 管道 + query 是值 | 可命名、可组合、可单测；比 SQL 子句嵌套更接近人的阅读顺序；事故可以在定义边界定位 |
| stage 即函数（subject-last） | 这是 `where (.x > 1)` 能变成可复用函数、`&` 能成立的原因；不要改成"关键字 + 特殊语法"，否则预置库没法用 Cagara 写 |
| `maybe` 默认非空 + 字段级可空 | 对 SQL 三值逻辑的正面回答；左连接后"哪些列可能 NULL"在类型里，而不是靠记 |
| phase 系统（聚合/窗口不嵌套、谓词非空） | 把一整类运行期错误变成编译期错误；这是 Cagara 最独特的价值 |
| 行类型贯穿 + 编译期拒绝未分组字段 | 报表最常见的错误（漏 `GROUP BY`、拼错列）在这里不成立 |
| `sql` 模板必须带签名 | 逃生口有类型边界，预置库可以完全用 Cagara 写；比"任意字符串拼接"安全得多 |
| 诊断给 `available:`/修法、`fmt` 幂等、`doc` 从签名生成 | 已达到开源语言里中上水位；是真实可用性的基础 |
| typed core → arena → erase → SQL 的分层与 `Origin` 保留 | 语义与后端解耦，增量/编辑器/多方言都能复用；**不建议重写** |

一句话：**复杂度大多花在刀刃上；要砍的是"第二套写法"和"没有产品化的外围"，
不是类型系统本身。**

---

## 2. P0：会静默出错，应该当 bug 修（而不是当"设计取舍"）

### P0-1 join 同名列静默丢弃右侧列

现状（实测，三表 join，`users` 与 `merchants` 都有 `name`）：

```cagara
main =
  orders
  & innerJoin users (.<user_id == .>id)
  & innerJoin merchants (.<merchant_id == .>id)
  & select { .id, name = .name, .amount }
```

生成：

```sql
SELECT t1.id, t2.name, t1.amount
FROM public.orders AS t1
INNER JOIN public.users    AS t2 ON t1.user_id = t2.id
INNER JOIN public.merchants AS t3 ON t1.merchant_id = t3.id;
```

`t3` 参与 join，但 `t3.name` 一列都没进 SELECT；`--types` 只显示一个
`name = string`。想显式取右侧会被拒绝（`.<x`/`.>x` 只能出现在 join 谓词），
于是**右侧列在语言层面不可达**。这是最容易造成"数错了还不知道"的一类问题。

**改法（分两层）：**

1. **短期（S）**：join 输出出现同名列 → 编译错误，错误信息给出两个改法：
   ```text
   error: join outputs two columns named `name`: one from `users`, one from `merchants`.
     rename before joining:  & select { user_name = .name, ... } & innerJoin ...
     or keep both:           users.name / merchants.name
   ```
   至少让静默变显式。
2. **中期（L）**：join 重新建模为"别名 × 命名空间 × 基数 × 显式摊平"：
   ```cagara
   from orders as o
   |> join users as u on o.user_id == u.id
   |> join merchants as m on o.merchant_id == m.id
   |> select { o.id, u.name, m.name, o.amount }
   ```
   `m.name`/`u.name` 是不同名字，冲突消失；需要单行输出时显式 `select { ... }`
   摊平。参考 Malloy 的 `join_one`/`join_many` + 命名空间、PRQL 的
   `p = positions` + `this/that`。这条是破坏性改动，但属于"现在改成本最低"。

### P0-2 `limit`/`offset` 的 pipeline 语义 ≠ SQL 语义，且文档示例是错的

现状：

```cagara
page2 = orders & order [asc .id] & limit 20 & offset 20   # docs/LEARN.md:696
```

实际语义是"先取 20 行，再跳过 20 行"= **0 行**；生成的 SQL 是

```sql
SELECT ... FROM (SELECT ... ORDER BY id LIMIT 20) AS t1
ORDER BY id OFFSET 20;
```

SQL 用户理解的 `LIMIT 20 OFFSET 20` 是"跳 20 取 20"= 20 行。`CAGARA.md`
§5.5 的例子（`limit 20 & offset 40`）同样恒空。这是**文档级 + 语言级**双重陷阱。

**改法：**

1. **立刻（S）**：改文档两处示例为 `offset 20 & limit 20`；并加一条明显警告：
   "stage 按顺序执行，`limit` 在前会截断 `offset` 的来源"。
2. **短期（S）**：对"`limit` 后面紧邻 `offset`"给出诊断：
   `offset after limit is almost always a mistake; write offset then limit, or use page`.
3. **中期（S–M）**：加单一 `page { size = 20, offset = 20 }` stage（或
   `take 20 skip 20`，`skip` 先作用于行），把两个 stage 融合成一个 Rel 节点，
   SQL 直接出 `LIMIT 20 OFFSET 20`。顺便修掉现在"内外两层 ORDER BY"的冗余形状。

### P0-3 nullable join 键：自然写法报错，唯一可行写法是错的

三层 left join，第二层的键是第一层外连接的可空列：

```cagara
t1 = a & leftJoin b (.<id == .>aid)       # aid 变为 maybe int
main = t1 & leftJoin c (.<aid == .>bid)
```

报：

```text
error: left join input: field `aid`: expected a non-null value, found a `maybe`;
       use `coalesce default x` ...
```

按提示写成 `.<aid ?? 0 == .>bid` 后，SQL 是：

```sql
LEFT JOIN s.c AS t3 ON COALESCE(t2.aid, 0) = t3.bid
```

这会把 `bid = 0` 的真实行错误匹配到 NULL 扩展行上，**静默错结果**。
正确语义其实是 SQL 自己的语义：`NULL = anything` 不匹配，即
`ON t2.aid = t3.bid` 就是对的。类型系统不应把 join 谓词的 `=` 当成普通
表达式 `==`。

**改法（S–M，收益很大）：**

1. join 谓词的 `==` 允许 `maybe` 操作数，直接生成普通 `=`，并在文档写明
   "NULL 永不匹配（SQL 语义）"；需要 NULL=NULL 语义时显式 `eqMaybe`。
2. 保留普通表达式 `==` 的非空要求（这是对的）。
3. 错误信息里给 `eqMaybe` 与"join 键按 SQL 语义比较"两个提示，而不是
   一律建议 `coalesce`（在 join 键上建议 `coalesce` 本身就是错的）。

### P0-4 未知类型名静默变成自由类型变量

```cagara
orders : query { amount = decimal } = table "public" "orders"   # 0 报错
```

`decimal` 不在 `CONS` 里，`crates/cagara-hir/src/check/infer.rs` 会
`rigid()` 它，于是它成了"每个使用处重新实例化的自由变量"。后果：
`--types` 原样打印 `amount = decimal`；同一列可以被三个定义分别读成
`int`/`string`/`float` 且全部通过（`upper .amount` 会发出 `UPPER(amount)`）。
因为表签名是唯一 schema 来源、又没有内省校验，这类拼写错误**永远到运行期**。
`docs/PLAN.md` 已把它列为"需要语言变更"的边界，但它的危害值得提前处理。

**改法：**

1. **短期（S）**：签名里未定义的类型名 → 编译错误，附 `did you mean int /
   float / string / bool / date / timestamp / decimal?` 与已知类型列表。
   单字母或 `a`–`z` 的合法类型变量单独放行（沿用现有约定），其余必须报错。
2. **中期（M）**：显式 `forall a.` 绑定，或只允许约定好的变量命名集合；
   同时给 `symbol k`/`valueLabel w` 一个可见的说明，避免它们泄漏到用户签名。

### P0-5 除法/取模/溢出：类型系统在 SQL 语义上撒了谎

```cagara
main = t & select { d = .a / .b, m = .a % .b }   # d : int
```

SQLite 实测：`7/0` → `NULL`，`7%0` → `NULL`，`INT64_MAX+1` → `9.22e18`（REAL）。
T-SQL 版本还会生成 `a / NULLIF(b, 0)`，主动把除零变成 NULL；Postgres 则
报错。类型系统说 `int`，后续 `where (.d > 0)` 会静默丢行，且无法 `coalesce`
（它不是 `maybe`）。

**改法（S–M）：**

1. `/`、`%` 的结果改为 `maybe int`（或在类型里显式区分 `div`/`mod`），
   后端统一生成 `NULLIF(b, 0)`，让"除零"成为显式可处理值；`float` 除法同理
   需要方言一致的策略（PG 报错 vs MySQL NULL/Inf）。
2. 提供 `divUnsafe` / `modUnsafe` 给"确定非零"的热路径，文档写清风险。
3. `engines.rs` 增加除零、负零、`INT64_MAX+1`、`abs(INT_MIN)` 等用例；
   目前 live-engine 测试只覆盖 SQLite/DuckDB，且 optimize 路径 0 覆盖。
4. 整数溢出无法在类型里完全表达；至少文档不要承诺"`int` 是精确的"，并在
   方言矩阵里标注每个引擎的行为。若目标里有 Postgres/MySQL，溢出是报错，
   静默出错的风险主要来自 SQLite，引擎测试能把它锁住。

### P0-6 LSP 与 CLI 的诊断口径不一致

`crates/cagara-lsp/src/analysis.rs:30` 只保留 root 文件的诊断：

```rust
for d in compilation_diagnostics.iter().filter(|d| d.path == root) {
```

实测：`proj/sub/q2.cagara` import 的 `schema_bad.cagara` 有错，CLI `exit 1`
并打印 `schema_bad.cagara:2:5: error: unknown name 'nope'`；同一工程用 LSP
打开 `q2.cagara` 时 `publishDiagnostics` 为 `[]`。在 733 个文件互相 import
的真实工程里，这就是"编辑器全绿、CI 红"。

**改法（S）：** 发布 root 可达闭包内的全部诊断（LSP 协议本身支持跨文件
URI），并按文件分组；同时把"当前文件 vs 工作区"分开关，避免一次刷屏。

### P0-7 方言覆盖把"能生成"当成"能运行"

`--dialect` 接受任何 `sqlglot-rust` 认识的方言名。实测：

| 方言 | 生成 | 问题 |
|---|---|---|
| `oracle` | `DATE_TRUNC('MONTH', occurred_at)` | Oracle 无 `DATE_TRUNC` |
| `hive` | `... LIMIT 20 OFFSET 20` | Hive（旧版本）无 `OFFSET` |
| `clickhouse` | ANSI/Postgres 回退 | 0 条测试断言 |

`docs/SQL-DIALECTS.md` 已经诚实分层（executed / rewritten / sqlglot-only），
但 CLI 没有把"sqlglot-only"告诉用户。`validate_dialect_support` 只证明
sqlglot 认识语法，不证明目标引擎能执行。

**改法（S–M）：**

1. CLI 引入方言分级：`verified`（有引擎测试）/`rewritten`（有 intrinsic 重写）
   /`generated-only`。选到第三类时 stderr 打一行醒目警告，或要求
   `--allow-unverified`；不要静默。
2. 把 `oracle`/`hive`/`clickhouse` 降级，或补 intrinsic 与引擎测试；
   CI 至少对"声称 verified"的方言跑容器/临时实例。
3. `ansi` 这个名字不副实：默认输出含 `DATE_TRUNC`、`LIMIT`、`NULLS LAST`，
   不是 ANSI。建议改叫 `generic`/`postgres`，并把真正的 ANSI 输出作为单独目标。

### P0-8 `lpad`/`rpad` 的文档与模板矛盾（并且 `sql` 模板不检查参数语义）

文档（`docs/LEARN.md:1451`、`CAGARA.md:1110`、`prelude.cagara:614`）说
`lpad n fill s` → `LPAD(s, n, fill)`；模板是：

```cagara
lpad : expr r int -> expr r string -> expr r string -> expr r string = sql "LPAD($2, $1, $3)"
```

即实际参数序是 `width, subject, fill`，与文档的 `width, fill, subject` 相反。
实测 `lpad 10 (toString .n) "0"` 才得到 `LPAD(n, 10, '0')`，而按文档写
`lpad 10 "0" (toString .n)` 会得到 `LPAD('0', 10, n)`。`res/icmidle` 的移植
已经按实现顺序改了 5 处调用，说明这不是理论问题。

**改法（S）：**

1. 对齐二者：按 subject-last 原则，模板应改为 `LPAD($3, $1, $2)`；
   若要保留现有调用，提供 `lpadTo width subject fill` 过渡并弃用旧序。
2. 给 `sql` 模板加一类轻量测试：文档里的示例调用必须与生成的 SQL 对照
   （现在 `sql` body 不被检查，签名只能约束类型、约束不了参数顺序）。
3. 这条也说明 `sql` 逃生口"半透膜"：需要 `raw`/`dialect-specific` 模式，
   否则方言专属函数在 sqlglot 重写后仍可能被改写/丢语义。

> **复核说明**：`CAGARA-BUGS.md` 第 6 条 `dayOfWeek` 方言不一致，在目前能
> 核对的映射下不成立——`ansi`/`postgres` 生成 `EXTRACT(DOW)`（Postgres 语义 0=Sunday），
> `sqlite` 用 `STRFTIME('%w')`（0=Sunday），Trino/Spark/MySQL 都有正确的
> `%7`/`-1` 修正，TSQL 还处理了 `@@DATEFIRST`。这条应从 bug 清单删除或重测；
> 它提醒：那份 bug 清单本身也需要 triage，避免后人追错问题。

---

## 3. P1：每天都会遇到，有绕法但绕得难受

### 3.1 表面语法：两套写法 + 一套 Haskell 记忆

**符号简写家族**（`&? &= &+ &* &. &- &| &! &^ &~` 与 `? <? ?> <?>`）
是净负担：

- 文档自己承认"符号无法从名字推断，这张表是它们被列出的唯一地方"
  （`docs/LEARN.md:197`），且 `&^` 曾连续多个版本被文档写成 `offset`。
- `&=`（替换整行）与 `&+`（合并/追加）只差一个字符，语义相反；`update`
  改名还会静默产生 `age` + `age AS years` 两列（没有 `rename`）。
- 实测 `&^ 10` 只报 `expected query { id = int }, found int`，不提示
  "`&^` 是 intersect，`offset` 没有简写"。
- 同一份报表在 `examples/report.cagara` 里写了两遍（长名版 + 简写版），
  这本身就是"两套写法"的成本证据。

**建议（S，破坏性但值得）：** 具名 stage 是唯一正式写法；符号简写先标
deprecated（`fmt --canonical` 自动迁移），一个 minor 后删除。保留 `&`
作为管道（或加 `|>` 别名），join 的符号族一起删。参考 PRQL：每行一个具名
transform 同样紧凑，却不需要查表。

**`select` / `update` 的命名**：`select` 是"替换整行"（projection），
`update` 是"合并列"（extend/derive）。对 SQL 用户，`update` 是 DML，直觉
完全不同。建议改名 `project`/`extend`（或 `select`/`derive`），并把
"rename" 做成独立 stage：

```cagara
# 现在：保留 age，又追加 years（静默双列）
q & update { years = .age }
# 建议：
q & rename { years = .age }
q & extend { years = .age }     # 显式：age 仍在
```

**`<>` 是字符串拼接，而 SQL 用户读作"不等于"**：实测
`where (.plan <> "free")` 报 `expected bool, found string`，不提示用 `!=`。
建议把拼接改成 `++`/`||` 之一，`<>` 与 `!=` 统一为不等（或删除 `<>`）；
这是最容易让 SQL 老手写错的一处。

**`case`**：分隔符是 `;` 不是 `,`，必须字面量 `true =>` 兜底；条件以名字
结尾时会贪婪吞成 lambda——实测 `case { x > start && x < finish => true; ... }`
报 `expected => between a case's condition and its value`。建议改为
`case { cond -> value; ...; else -> default }`（或强制每条 condition 加括号，
`fmt` 自动加），至少错误信息要教人。

**`[..]` 是"固定 key 列表"不是通用列表**：`[1, 2.0]` 是类型错误、不能
`map`/拼接、只在 order/spec/inList 位置有效。这个"半个列表"是设计毛边。
要么做真列表（成本高），要么换语法：`order asc .x, desc .y`、`partition by .x`，
把 `inList [..]` 变成 `in (..)` 关键字；不要让它看起来像 Haskell 列表。

**中文列名 / 带空格列名不可书写**（P0 对中文团队）：实测
`query { 总金额 = float }` 与 `` .`total amount` `` 都是语法错误；表名可以
（它是字符串），列名不行。真实工程用 `## 网点号` 把中文放进注释——但
"注释里的列名"不能参与补全/跳转/错误信息。建议：标识符允许 Unicode
与反引号引用，后端按方言引用；这是很多中文数据团队能否采用的前置条件。

**其它小而真实的毛边：**

| 问题 | 现状/证据 | 建议 |
|---|---|---|
| 无块注释 | 只能每行 `#`，关掉一段 pipeline 很痛苦 | 加 `{- ... -}` |
| 日期字面量 | `"2024-01-01"` 的加宽取决于上下文是否有唯一期望类型；`addDays 7 "..."` 报 overload 错误 | `date"..."`/`timestamp"..."` 字面量；`addDays` 接受字面量 |
| 日期分桶 | 只有 `truncMonth` 等；`group (month .ts)` 编译通过但按 `EXTRACT(MONTH)` 分组，**跨年同月静默合并** | `dateTrunc("month", x)`/`datePart`；对 `group (month ...)` 加 warning |
| 无 NULLS FIRST | `order [asc .v]` 对 `maybe` 直接报错，且 `asc/desc` 无法控制 null 顺序 | 允许 nullable 排序键 + `nullsFirst`/`nullsLast` 修饰 |
| 顶层常量不能计算 | `x : int = 40 + 2` 报 `expected int, found expr a b`；只有字面量能当常量 | 允许纯常量表达式（给 `expr r a` 一个无行占位）或文档明确 |
| `--only` 与 scalar | `--only x` 对 `x : int = 42` 报 "no query definition named x"，与 CAGARA 文档"任意类型"不符 | 统一行为或改文档 |
| doc 注释贴行 | `##` 与定义之间隔一行普通注释就静默脱落，`doc --check` 才提示 | 允许空行/普通注释；或 `doc --check` 进 CI |

### 3.2 类型与可空性：方向对，糖严重不足

**"coalesce 税"**：同一张报表，inner join 版 0 处 null 管道，left join 版
十几处 null 管道（`02-types.md` 对同一报表的逐项统计）。`res/icmidle/src`
4,207 行里有 104 个 `coalesce`、
54 个 `leftJoin`，约每 40 行一个。根因是 `sum`/`avg`/`min`/`max` 在所有
输入上都返回 `maybe`，以及 outer join 后列级 `maybe` 没有收窄。

分层建议：

1. **聚合精度（S–M）**：`agg` 的每个分组至少一行；若聚合输入是非空列，
   `sum`/`avg`/`min`/`max` 在该分组内**不可能为 NULL**。现在
   `sum .amount`（`amount` 非空）仍返回 `maybe`，迫使
   `coalesce 0.0 (sum .amount)`。给非空输入一个非空重载，或在 `agg` 内
   消掉 `maybe`，能直接删掉报表里最大的一块噪音。
2. **流敏感收窄（M）**：`where (isNotNull .x)` 之后，同一 pipeline 内的
   `.x` 应视为非空（同理 `whereTrue`、`ifThenElse` 分支）。现在收了也白收。
3. **分支合并（S）**：`ifThenElse p 0 .b`（一支非空、一支 `maybe`）报错，
   必须两边都 `just`。应允许"非空提升为 maybe"（`a` 与 `maybe a` 在各分支
   统一到 `maybe a`），这是 HM 无子类型导致的多余摩擦。
4. **命名清理（S）**：`countOf` 要求非空、`countOfMaybe` 是显式可空版，
   命名分裂；`nothing` 会让定义类型不闭合（`x = nothing` 推出自由 `maybe a`）。
   建议统一 `countOf : expr r (maybe a) -> int`（SQL 本来就跳过 NULL），
   或提供 `countNonNull`；`nothing` 必须有期望类型，否则报错。
5. **可空 join 键**：见 P0-3，优先修。

**参数化查询的两个组合 bug（P0/P1 级，直接塑造了真实工程的写法）**：

- **同一个 lambda 参数出现在 join 两侧时，一侧的列会丢失**：

  ```cagara
  x = pd => a & where (.pt_dt == pd)
  y = pd => b & where (.pt_dt == pd)
  j = pd => x pd & leftJoin (y pd) (.<k == .>k2)
  # error: no column `k`; available: pt_dt, k2, name   ← 左侧整行没了
  ```

  报告里这条"shapes every ported module"：`innerJoin`/`leftJoin`/`fullJoin`/
  `semiJoin`/`antiJoin` 与集合运算都会中招。绕法是"把 query 作为参数传入"，
  于是真实代码写成 `average_balances_of deposit.edw_..._balances (branch_organs d) (active_customers d)`
  而不是 `average_balances_of d`。这是可复用性上的硬伤。
- **定义内用了自己的参数、又经过 projection 后，参数不再可见**：

  ```cagara
  f = pd => a & where (.k == pd) & select { k2 = .k } & where (.k2 >= pd)
  # error: no column `k2`; available: k, status
  ```

  绕法是把参数提前过滤、或重新从源表取。这同样限制"参数化定义"的写法。

这两条属于类型/解析层的实现缺陷（参数的类型绑定在声明行上，不随 stage
行变化），不是"应不应该有"的设计问题。建议优先修：参数引入时应当是
row-polymorphic/按当前位置实例化，而不是固定成声明时的行；否则"可复用
stage"这个卖点会持续漏气。

**类型语法/诊断**：

- 打印出来的类型不可回写（`removeKey(k, r)` / `mapValue(a, ...)` 不在
  签名语言里），`--types` 的行顺序与 SQL 列顺序不一致。
- 诊断泄漏内部机制：`kind mismatch: cannot unify Type with Row`、
  `type is too deeply nested to unify; a keyMap term may reduce in a cycle`
  （合法程序也能触发，见报告例）、`symbol`/`valueLabel` 出现在用户签名里。
  建议：错误信息只写用户语言（"join 谓词漏了一侧的 `.`"），内部类型名
  只在 `--debug-types` 下出现。
- 没有用户类型/别名/枚举：`status` 是裸 `string`，join 键都是 `int`，
  拼错/传错列不会报错。建议按性价比加：行类型别名（`type Orders = {...}`）
  → 枚举/域类型（`plan : enum`）→ decimal/interval。不要加泛型/type class。

### 3.3 表达力缺口：按"日常报表真正写不出来"排序

先修正一个常见假设：**"相关子查询/EXISTS 写不出来"不成立**——
`semiJoin`/`antiJoin` 已经生成相关 `NOT EXISTS`，`rowNumber + where` 能写
top-N per group，`sumOver` 能写占比/累计。真正缺的是下面这些：

| 缺口 | 现状 | 建议（成本） |
|---|---|---|
| `HAVING` | 聚合后 `where` **100% 派生表**（`crates/cagara-sql/src/lower.rs:346` 有意为之）；只用于过滤的聚合列也被迫物化 | 加 `having` stage（显式、可读），并在 Rel→Rel pass 里折叠；与 `where` 的差异写清（S–M） |
| 分页 | 见 P0-2 | 单一 `page`/`take&skip`（S） |
| `GROUPING SETS`/`ROLLUP`/`CUBE` | 完全没有；报表常用的"小计/合计"写不出 | 加 `agg` 的 grouping-set 形式（L） |
| `DISTINCT ON` / top-1 per group | 要两层 stage | 加 `distinctOn` 或 `take 1` per group 糖（M） |
| 精确 decimal | `toDecimal 18 2` 只改 SQL CAST，语言类型仍是 `float`，还能与 double 混算 | 加 `decimal(precision, scale)` 标量类型；至少让 decimal 值不能静默参与 float 运算（M） |
| 绑定参数 | 完全没有；真实工程用 `${user_input_date}$` 文本占位，靠宿主替换 | 加 `?`/`:name` 参数与 `--param name=value`，输出 `$1`/`?`；这同时解决注入与复用（M） |
| 常用聚合/函数 | 无 `median`、`stddev`、`string_agg`、`array_agg`、`bool_and/or`、`split`、`regexp_*`、`json` 访问、`interval` | 按使用频率补 prelude；`sql` 模板目前只能做"整个定义体"，内联表达式需要额外签名，建议支持内联 `sql`（M） |
| `FILTER (WHERE ...)` | 预置库用 `CASE` 模拟，结果正确但 SQL 冗长 | 有方言支持时生成真 `FILTER`（M，可选） |
| 集合运算按名对齐 | 要求名字+顺序+类型完全一致；顺序不同只报"must have the same columns" | 名字相同、顺序不同时自动按名对齐；`unionByName` 显式版（S） |
| 递归 CTE / LATERAL | 不支持（递归可用 200 层展开绕，且 200 层就撞 budget） | `PLAN.md` 已列；按需再评估（L） |
| `NULLS FIRST` | 不支持，且排序键强制非空，`NULLS LAST` 路径实际空转（`docs/CORE.md` 的承诺用不上） | 允许 nullable 排序键 + 修饰符（S） |

另外两点：

- **`--optimize` 近乎空转**：12 个探针里只有 `WHERE TRUE` 被删掉；而
  `docs/LEARN.md:1939` 承诺 constant folding / boolean simplification /
  pushdown；`engines.rs` 里 optimize 出现 0 次、LSP 不暴露该 flag。
  建议：要么真正接入差分执行测试再宣传，要么先降级为 `--experimental-optimize`
  并从 README/LEARN 的承诺里移除。**不要保留"两套可观察 SQL"却没有验证。**
- **生成 SQL 的冗余形状**：集合运算多包一层 `SELECT ... FROM (...) t3`、
  分支内保留不可观察的 `ORDER BY`、`limit + where` 重复 `ORDER BY`、
  semi/anti join 左侧无条件派生。建议把"形状决策"从 `lower.rs` 抽成
  独立的 `Rel -> Rel` pass（`docs/PLAN.md` 已经提过），`lower.rs` 只做机械
  输出；这同时是 HAVING、CTE、派生表合并的统一入口。

---

## 4. 工具链与项目：从"编译器"补成"日常工具"

### 4.1 真实工程暴露的五件事

1. **表结构只能手写/外部生成**：`res/icmidle` 的 713 个模块、93,910 张表、
   3,069,063 个列，是 `tools/convert_tables.py` 从 `tables.jsonl` 生成的；
   Cagara 没有 catalog/连接/内省/`schema` 命令。生成器只保证"这次生成了"，
   不保证"和真实库一致"；LSP 也无法从 catalog 补全表/列。
2. **没有项目级入口**：`cagara proj` → `Is a directory (os error 21)`；
   没有 `check`/`build`/目录递归/清单；库文件静默 exit 0，"检查通过"与
   "什么都没做"不可区分；733 个文件只能 `xargs` 循环。
3. **没有机器可读输出**：编译侧没有 `--json`，SQL 是唯一产物；诊断无
   `code`/`related`/`fixes`；CI 只能抓 stdout 文本。
4. **失败仍向 stdout 写 SQL**：main 好、helper 坏时 exit 1 但 stdout 已有
   `-- main\nSELECT...`，`cagara x.cagara | sqlite3` 会执行半成品。
5. **冷启动/缓存**：`cagara src/account_information.cagara` = 1.35 s /
   236 MB；LSP `didOpen` → 首条诊断 = 1.37 s / 381 MB（依赖 9.4 MB 模块），
   每次保存预览都重付这份成本；CLI 无持久缓存。

### 4.2 建议（按收益/成本排序）

| 建议 | 具体动作 | 成本 |
|---|---|---|
| `cagara.toml` + 项目命令 | `paths = ["src/**/*.cagara"]`、`library = ["lib"]`、`dialect`、`fmt.width`；`cagara check [dir]` 递归、按文件汇总、`--json`、`--fail-on-warn`；退出码区分 1 程序错误 / 2 用法 / 3 编译器内部 | M |
| schema 管线 | `cagara schema --ddl schema.sql` / `--dsn postgres://…` → 生成或更新 schema 模块；`cagara check --schema schema.sql` 校验声明漂移；生成文件带 `#! generated` 标记；LSP 从 schema 模块补全表/列 | M–L |
| `cagara run` | 先接 DuckDB/SQLite（已经是测试依赖），`--limit`、`--json`、`--param`、`--explain`；让"改—看 SQL—看结果"闭环 | M |
| 多输出 | `--all`、`--out-dir`、或 manifest 的 `outputs`；现在一个文件只能 print 一个 `main` | S |
| 失败不留半成品 | 编译成功后才写 stdout；或 `--output` 原子写 | S |
| `--json` 编译输出 | `{ sql, types, diagnostics: [{code, range, message, fixes}], dialect, deps }`；`doc --json` 已有，保持一致 | S–M |
| LSP | 发布可达闭包诊断；code action（`coalesce`/`whereTrue`/`isNotNull`/`*Maybe`/`rename`/`prefix`）；import 行 go-to-def；`didChangeConfiguration` 读 dialect/width；缓存/后台索引；SQL 预览统一用项目 dialect | M |
| fmt 工程化 | 仓库自己的 `.cagara` 有 5 个过不了 `cagara fmt --check`（`corpus/{joins,stages,windows,rows,aggregates}.cagara`），CI 只跑 `cargo fmt`；加 `fmt --diff`、`--check` 进 CI、`cagara.toml` 的 width/indent | S |
| 缓存 | CLI 端持久化 `(mtime,len)` 解析/检查缓存或常驻 daemon；LSP 已有增量，但冷启动仍全量；至少让一次编译在 100 ms 级 | M–L |
| doc | `--check` 恒 exit 0，不能做门禁；加 `--fail-on-warn`/覆盖率；跟随 import；`doc -` 的标题与 `<stdin>` 统一 | S |

### 4.3 编辑器能力水位

现有：hover（含跨模块字段注释）、go-to-definition、find-references、
document symbols、formatting、completion（含 `.`/`.<`/`.>`）、`##` 文档。
缺少（对日常影响大的）：**quick fix / code action**（诊断已经知道修法，
却没有一键应用）、rename（列/定义）、signature help、inlay hints、workspace
symbol、range formatting、配置项。编辑器是这个语言最自然的入口，建议把
"诊断 → code action"作为下一个 LSP 里程碑；它能直接消灭 coalesce 税、
`.<`/`.>` 漏写、`<>` 误用等高频错误。

---

## 5. 架构、文档与测试（不是重写，是收口）

- **分层是对的，不要拆 crate**：6 crate / 38k LOC 对一个 0.3.2 的编译器
  是合理规模。真正要动的三处：
  1. `crates/cagara-sql` 的 API 只有 `compile() -> Result<String, String>`；
     方言等级、优化开关、错误类别（程序错误 vs 编译器能力缺口）无处表达。
     建议返回结构化 `BackendError { kind, origin }`，把 HIR 的
     `Fault::{Program, Compiler}` 延续到 SQL 层；`MySQL fullJoin`、16 层
     深度这类"能力缺口"应有独立文案与源位置，而不是混在 `error in 'x'`。
  2. `lower.rs` 混合"形状决策 + SQL 输出"；抽一个 `Rel -> Rel` 的 shape pass
     （计划里已有）。附带修：`normalize/budget.rs:59` 的
     `if self.depth >= MAX_DEPTH` 与文案 "more than 200" 差一（实测 199 通过、
     200 失败）；200 层无环别名链还会报 "internal error … recursion"，
     并产生 31 条级联诊断。根因聚合 + off-by-one 一起修。
  3. `check/infer.rs` 2,918 行、`check/tests.rs` 3,195 行，建议按
     "签名/实例化/重载/行约束"拆文件；`count_rel` 对每个 Rel 做线性扫描 +
     深比较 + clone 的最坏 O(N²)，大查询可换 interning/指纹桶。
- **文档重复已经在产生漂移**：README + `CAGARA.md`(1,768) + `LEARN.md`(2,135)
  + `CORE.md` + `NULLABILITY.md` + `ROW-TYPES.md` + `ARCHITECTURE.md` ≈ 6,700 行，
  讲同一批事实。已经出现三处不一致：`lpad` 参数序、`limit/offset` 示例、
  `&^` 历史误写。建议：
  1. `prelude.cagara` 的 `##` 是唯一函数文档源，用现有 `cagara doc --prelude`
     生成"函数参考"章节，不再手抄。
  2. `LEARN.md` 只留"人的教程"，`CAGARA.md` 只留"给 agent 的契约与示例"，
     两者引用同一份 generated reference；契约里的每一条都对应一个可执行测试。
  3. 给诊断加稳定 `code`（如 `CG0101 no-column`），文档/测试/LSP 共用。
- **测试**：`cargo test --workspace` 全绿是好的，但语料只有 8 个 corpus 文件 + 4 个 examples，
  相对语言表面积偏小；live-engine 只覆盖 SQLite/DuckDB，`--optimize` 0 覆盖。
  建议：把 `res/icmidle/src` 的报表（匿名化）纳入 corpus；每个方言声称至少
  一条 engine 测试；`fmt --check` 与 `doc --check` 进 CI；为 prelude 的
  `sql` 模板加"文档示例 → 生成 SQL"对照测试。
- **版本/稳定性**：0.3.2 pre-1.0 是改语言的最好窗口。建议明确一条
  "breaking 政策 + 弃用期"（例如：minor 标 deprecated、下一个 minor 移除），
  用 `fmt --canonical` 提供自动迁移；否则符号简写这类设计债会永久固化。

---

## 6. 路线图（按风险/收益排序）

### 阶段 A：0.3.x，"先把错数堵住"（1–2 周，低风险）

- join 同名列 → 编译错误（P0-1 短期）
- 修 `limit/offset` 文档 + 加诊断；文档示例同步修（P0-2）
- nullable join 键允许 SQL 语义（P0-3）
- 未知类型名 → 报错 + did-you-mean（P0-4）
- `/`/`%` → `maybe` + 引擎测试（P0-5）
- LSP 发布可达闭包诊断（P0-6）
- 方言分级警告（P0-7）
- `lpad`/`rpad` 文档-实现对齐（P0-8）
- CLI：失败不写 stdout、`--json`、`check <dir>` 最小版
- 仓库自己的 `.cagara` 过 `fmt --check`，CI 加 `fmt/doc` 门禁
- 文档：修 P0 示例、补 `<>`/`case`/`.<` 的醒目警告

### 阶段 B：0.4，"语言收紧 + 表达力"（1–2 个月，有破坏性）

- 具名 stage 为唯一写法，符号简写 deprecated + `fmt --canonical`
- `select`/`update` → `project`/`extend`（或保留 `select` + `derive`），
  加 `rename`；`<>` 改拼接符号；`case` 语法收紧
- `having` stage + Rel→Rel shape pass（顺带 HAVING、派生表合并、CTE）
- 单一分页 stage；`NULLS FIRST/LAST` + nullable 排序键
- `ifThenElse` 分支 maybe 合并；流敏感收窄；`sum` 非空重载；
  参数化查询两个组合 bug
- `decimal` 标量 + 绑定参数 + `dateTrunc`/日期字面量
- 集合运算按名对齐；`GROUPING SETS` 视需求

### 阶段 C：0.5，"产品化"（2–3 个月）

- `cagara.toml` + `cagara schema`（DDL/DSN）+ `check --schema`
- `cagara run`（DuckDB/SQLite 先行）+ `--param` + `--explain`
- LSP code actions / import 跳转 / 配置 / 缓存
- CAGARA/LEARN 文档去重，reference 由 prelude 生成
- 方言验证矩阵 + CI 容器；删除/实现 `--optimize`

> 现实一点：A 是必须做的；B 里"符号简写 + join 模型"二选一先做（两者都改
> 会让旧代码全碎）；C 的第一项（schema + 项目命令）是日常收益最大的。

---

## 7. 如果只能做五件事

1. **把静默错误变成编译错误**：join 同名列、`limit` 后的 `offset`、未知
   类型名、nullable join 键、除法/溢出、LSP 闭包诊断。这六项都不需要
   大重构，却能直接减少"数错了"的概率。
2. **砍掉第二套写法**：只留具名 stage，符号简写标 deprecated；`fmt` 负责
   迁移。同步修掉 `select`/`update`/`<>` 的 SQL 直觉冲突。
3. **补 SQL 形状**：`having`、单一分页 stage、nullable 排序键 +
   `NULLS FIRST`、集合运算按名对齐。让"日常报表"写出来直接是目标 SQL 形状。
4. **schema 从数据库/DDL 生成并校验**（`cagara schema` + `check --schema`）：
   这是真实工程里最大的摩擦，比任何语法糖都值钱；保持显式行类型作为
   "冻结模型"，不要取消。
5. **补工具回路**：`cagara check <dir> --json`、`cagara run`（DuckDB/SQLite）、
   LSP quick fix + 缓存。让"改—编译—看 SQL—看结果"从 1.35 s 降到可交互。

第 0 件事：**先决定 Cagara 的首要用户是谁**——写报表的分析师、生成代码的
agent、还是某个语义层的 typed IR？现在的很多毛边（符号简写、`CAGARA.md`
的语气、错误信息的两套受众、`main`/`--only`）都来自这个决定没有落地。
这个决定会直接决定上面哪些"建议"该做、哪些该删。

---

## 8. 需要作者拍板的问题

1. **受众**：daily 用户是数据同学，还是 LLM/代码生成器，还是"人 + agent 都要"？
   若是分析师，符号简写/Haskell 表面要大幅收敛；若是 IR/agent，应该把
   `CAGARA.md` 变成机器可读的 grammar + 紧凑参考，而不是 1,768 行散文。
2. **兼容性**：0.4 能不能破坏旧代码？如果能，符号简写/`select`/`update`/`<>`
   应该现在改；如果不能，就永远不要做"第二套写法"，只做加法。
3. **方言野心**：只保证 SQLite/DuckDB/Postgres（verified），还是"sqlglot 能
   生成就算支持"？前者需要删方言/加 warning，后者需要把承诺写清楚。
4. **schema 事实源**：继续"生成脚本 + 手写签名"，还是把 schema 变成 Cagara
   自己的产物（`schema.cagara` 由工具生成、CI 校验）？
5. **执行**：要不要 `cagara run`？不执行的话，类型系统对 SQL 语义的承诺
   （除零、溢出、decimal、方言）永远只能靠文档和引擎测试守。

---

## 附录 A：关键证据位置

| 主题 | 位置 |
|---|---|
| 架构与不变量 | `docs/ARCHITECTURE.md`、`docs/CORE.md` |
| 当前边界/延后项 | `docs/PLAN.md`、`CAGARA.md` §9.1 |
| 可空性规则 | `docs/NULLABILITY.md`、`docs/CORE.md` §4 |
| 行类型 | `docs/ROW-TYPES.md` |
| 方言分层 | `docs/SQL-DIALECTS.md` |
| 性能/增量设计 | `docs/PERFORMANCE.md` |
| 算符与简写表 | `crates/cagara-syntax/src/ops.rs:84-127` |
| join 输出"左胜" | `docs/LEARN.md` §9、`docs/CORE.md` §4 |
| HAVING 有意不做 | `crates/cagara-sql/src/lower.rs:346`、`docs/PLAN.md` |
| 未知类型名 → 自由变量 | `crates/cagara-hir/src/check/infer.rs:638,886` |
| 深度 off-by-one | `crates/cagara-hir/src/normalize/budget.rs:59` |
| LSP 只发 root | `crates/cagara-lsp/src/analysis.rs:30` |
| `lpad` 文档/模板 | `prelude.cagara:629`、`docs/LEARN.md:1451`、`CAGARA.md:1110` |
| 分页错误示例 | `docs/LEARN.md:696`、`CAGARA.md` §5.5 |
| optimize 承诺 | `docs/LEARN.md:1939` |
| `NULLS LAST` 空转 | `docs/CORE.md` §4、"NULLs sort last" 段 |
| 真实工程 | `res/icmidle`（733 文件、713 模块、93,910 表、3,069,063 列） |
| 作者自记 bug | `res/icmidle/CAGARA-BUGS.md`（其中 dayOfWeek 一条经复核不成立） |
| 并行评审原文 | `.cagara-review/01..05*.md`（3,169 行） |

## 附录 B：一键复现

```sh
C=/home/sosoy/Documents/code/icmidle/res/cagara-lang/target/release/cagara
S=/home/sosoy/Documents/code/icmidle/.cagara-review

# join 同名列静默丢弃
$C $S/scratch-surface/t02_shadow_wrong.cagara --types

# 分页恒空（文档示例）
$C $S/scratch-lead/page.cagara

# nullable join 键：报错与错误绕法
$C $S/scratch-lead/bugK.cagara
$C $S/scratch-lead/bugK2.cagara

# 未知类型名
$C $S/scratch-types/01-typo.cagara --types

# 除法/溢出（SQLite）
$C $S/scratch-lead/intdiv.cagara | sed 's/analytics.invoices/main.t/' > /tmp/d.sql
sqlite3 :memory: 'SELECT 7/0, 7%0, 9223372036854775807 + 1;'

# 方言虚假覆盖
$C $S/scratch-lead/dial.cagara --dialect oracle
$C $S/scratch-lead/page2.cagara --dialect hive

# lpad 文档 vs 实现
$C $S/scratch-lead/bugG2.cagara

# case 条件贪婪
$C $S/scratch-lead/bugE.cagara

# 参数化查询丢列
$C $S/scratch-lead/bugC.cagara
$C $S/scratch-lead/bugD.cagara

# 聚合后过滤的 SQL 形状
$C $S/scratch-lead/try4.cagara

# 全仓测试
cd res/cagara-lang && cargo test --workspace
```