# SQL 逻辑的表达：从关系演算到 U-semiring 与结构化递归

> 研究问题：在 pure FP 的前提下，**怎样更精确、更可组合地表达 SQL 的逻辑**，
> 而不是只做一个 SQL 语法糖。本文做四件事：
>
> 1. 把 SQL 的逻辑拆成七个可独立形式化的层面；
> 2. 比较现有理论的覆盖范围（关系演算、U-semiring、NRC/结构递归、
>    monad comprehension、comonad、graded/coeffect、provenance、Datalog、精化）；
> 3. 提出一个综合演算 **SQL-λ**（semiring-valued relations + monoid folds +
>    ordered/comonadic presentation + 3VL boundary + graded/refinement types）；
> 4. 讨论逻辑等价性的可判定性、工具与研究议程。
>
> 结论先行：**SQL 的逻辑不是一阶逻辑，也不是单纯的 bag 关系代数；
> 最忠实的现有基础是 U-semiring（bag multiplicity + unbounded summation +
> duplicate elimination），最纯 FP 的类型化基础是 NRC/结构递归，
> 最接近真实 SQL（NULL/名字绑定）的参考语义是 Guagliardo–Libkin。
> 把三者与 comonad/order、3VL、graded/refinement types 组合，才是
> "更好表达 SQL 逻辑" 的方向。**

---

## 0. 摘要：SQL 的逻辑由七个层面组成

| 层 | SQL 构造 | 逻辑/代数结构 | 现有最好的理论 |
|---|---|---|---|
| 1. 关系核 | `FROM/WHERE/SELECT/JOIN` | 正存在逻辑 + 关系代数；bag 权重 | U-semiring / 关系演算 / NRC |
| 2. 重复/集合 | `UNION ALL` vs `UNION`、`DISTINCT` | 半环加法 + duplicate elimination | U-semiring |
| 3. 聚合 | `GROUP BY`、`SUM/COUNT/AVG`、`HAVING` | 商 + 半群/幺半群折叠 | NRC 的 summation / structural recursion；我们的 `Fold1/Fold0` |
| 4. 顺序/窗口 | `ORDER BY/LIMIT/OFFSET`、窗口函数 | 有序序列 + comonad | list 代数 + Uustalu–Vene comonad |
| 5. NULL | 三值逻辑、`IS NULL`、`NOT IN` | Kleene 3VL / option 边界 | Guagliardo–Libkin 的真实 SQL 语义 |
| 6. 递归 | 递归 CTE、`WITH RECURSIVE` | 最小不动点 / Datalog | 不动点逻辑、Datalog |
| 7. 值域/类型 | schema、decimal、enum、单位 | row types + refinements + grades | DataKinds/GADT/type family；Liquid/精化 |

**核心论点：**

> SQL 的关系核 = **semiring-valued 正存在逻辑**（U-semiring）；
> 聚合 = **monoid fold over non-empty groups**；
> 顺序/窗口 = **comonadic presentation**；
> NULL = **3VL boundary**（不是 Maybe 能完全替代的）；
> 递归 = **fixpoint**；
> 类型 = **row + phase grade + refinement**。
>
> 把这七个层面混成一个类型系统，必然 ad-hoc；把它们分层并给出各自定律，
> 才能既精确又可组合。

---

## 1. 为什么"一阶逻辑 + 集合"不够

### 1.1 关系演算 = 一阶逻辑（集合语义）

Codd 的关系演算把查询看作一阶公式：

```text
{ (x1,...,xn) | φ(x1,...,xn) }        -- φ 是一阶公式
```

`SELECT-FROM-WHERE` 是它的正存在片段；`EXISTS`/`NOT EXISTS`/`IN` 是量词；
SPJ 的关系代数与关系演算等价（Codd 定理）。集合语义下，
**CQ containment 是 NP-complete**（Chandra–Merlin 1977），这是等价性理论的起点。

但 SQL 不是集合语义：

- SQL 是 **bag**（多重集）；`SELECT` 不去重；`UNION ALL` 保留重复。
- `GROUP BY`/聚合**不是一阶逻辑**：`SUM/COUNT/AVG` 超出了一阶可定义性
  （计数/求和）。
- `ORDER BY/LIMIT` **不是关系算子**：它们改变可观察结果，破坏交换律。
- `NULL` 是 **三值逻辑**，`NULL = NULL` 是 unknown，`NOT IN`/`EXISTS`
  的语义依赖于此。
- 递归 CTE 是**不动点**，不是一阶。

所以"把 SQL 塞进一阶逻辑"必然要丢东西。正确的做法是**分层**。

### 1.2 SQL 的逻辑更接近"semiring-valued 逻辑"

如果给每个输出元组一个**权重**（multiplicity），关系就变成函数：

```text
R : Tuple -> Semiring
```

- 集合语义：权重 ∈ `Bool`（0/1）；
- bag 语义：权重 ∈ `Nat`（多重集）；
- provenance：权重 ∈ 多项式半环；
- 概率：权重 ∈ `[0,1]`（但概率需要额外约束）。

于是：

- `UNION ALL` = 逐点加法；
- `JOIN` = 带条件的乘法（卷积）；
- `WHERE` = 乘以 0/1 指示函数；
- `SELECT`（投影）= 对隐藏列求和；
- `DISTINCT` = duplicate elimination（support）；
- `GROUP BY` = 按 key 分组后对组内权重做 fold；
- `ORDER BY` = 另一个层（list）。

这就是 **U-semiring** 的核心思想。它不是"另一种语法"，而是 SQL bag 语义的
**代数化逻辑**。

---

## 2. 现有理论的覆盖矩阵

| 理论 | 关系核 | bag/重复 | 聚合 | 顺序/窗口 | NULL/3VL | 递归 | 类型化 | 等价性 | 定位 |
|---|---|---|---|---|---|---|---|---|---|
| 关系代数/演算（集合） | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ | CQ NP-complete | 语义基础 |
| **U-semiring** | ✓ | ✓ | 部分/需扩展 | ✗ | ✗ | ✗ | 半 | U-expression + SMT | **bag 逻辑核心** |
| **NRC/结构递归** | ✓ | 集合版；bag 需扩展 | ✓（summation） | ✗ | ✗ | 有限/可加 | ✓ | 研究型 | **纯 FP 类型化演算** |
| Monad comprehension | ✓ | bag 可做 | ✗ | ✗ | ✗ | ✗ | ✓ | monad 律 | 语法/组合 |
| Comonad/ordered | ✗ | list | 部分 | ✓ | ✗ | ✗ | ✓ | list/comonad 律 | 呈现层 |
| Graded/coeffect | — | — | phase/基数 | — | — | — | ✓ | — | 类型层 |
| Provenance semiring | ✓ | ✓ | 聚合 provenance | ✗ | ✗ | ✗ | 半 | 半环等式 | 语义/血缘 |
| 范畴数据库/CQL | ✓ | ✗ | ✗ | ✗ | ✗ | — | ✓ | — | schema/迁移 |
| Datalog/不动点 | ✓ | 集合 | 聚合扩展 | ✗ | ✗ | ✓ | ✓ | 片段可判定 | 递归 |
| 精化/依赖类型 | — | — | `NonEmpty` | — | `Maybe` | — | ✓ | — | 类型正确 |
| Guagliardo–Libkin | ✓ | ✓ | 部分 | ✗ | ✓ | ✗ | 半 | 参考语义 | **真实 SQL 语义** |

**读法：**

- 没有一个理论覆盖全部；**U-semiring + NRC + comonad + 3VL + fixpoint + types**
  是覆盖最全的组合。
- U-semiring 是 **bag/重复/投影求和** 的最佳逻辑；
- NRC/结构递归是 **聚合 + 类型化 + 纯 FP** 的最佳演算；
- comonad 是 **顺序/窗口** 的最佳结构；
- Guagliardo–Libkin 是 **真实 SQL 的 NULL/名字绑定/bag** 的参考语义；
- Datalog/不动点补**递归**；
- 精化/graded 补**类型正确**。

---

## 3. U-semiring：SQL bag 语义的逻辑

### 3.1 定义（概念版）

一个 **U-semiring** `(A, 0, 1, +, ×, Σ, D)`：

- `(A, 0, 1, +, ×)` 是交换半环；
- `Σ` 是**无界求和**：对任意（有限/无限）索引集 `I`，
  `Σ_{i∈I} a_i ∈ A`；满足空和 = 0、单点 = `a_i`、分配律
  `a × Σ_i b_i = Σ_i (a × b_i)`；
- `D` 是 **duplicate elimination**：`D(0)=0`，`D(a) ≠ 0` 当 `a ≠ 0`，
  `D(D(a))=D(a)`，且与 `+`/`×` 有支持律（见论文）。

**关系**是有限支撑函数 `R : Tuple -> A`；查询是 U-expression：

```text
q ::= 0 | 1 | q1 + q2 | q1 × q2 | Σ_i q_i | D(q) | ...
```

SQL 的翻译（概念版）：

```text
FROM/WHERE/SELECT        ->  Σ_{bindings} ∏ [predicates] × [projection]
UNION ALL                ->  q1 + q2
UNION / DISTINCT         ->  D(q1 + q2) / D(q)
JOIN                     ->  q1 × q2 × [join predicate]
```

语义上，`UNION ALL` 是半环加法，`JOIN` 是乘法，`WHERE` 是指示函数乘法，
`SELECT` 是对隐藏变量求和，`DISTINCT` 是 `D`。这精确对应 bag 语义。

### 3.2 为什么它是"逻辑"而不是"实现技巧"

- 它把 SQL 的 bag/重复/投影求和变成**等式理论**；
- 等价性 = U-expression 在 U-semiring 公理下的相等；
- 可用 **UDP**（U-expression decision procedure）或 SMT 判定一个相当大的片段；
- Cosette/QED 等工具正是基于 U-semiring 建模 SQL 的 bag 语义
  （Cosette 还处理约束/键/外键）；
- 它解释了为什么 `UNION ALL`、`DISTINCT`、投影、join 重排有定律，
  而 `ORDER BY` 没有。

### 3.3 边界

- **聚合**：`SUM/COUNT/AVG` 不是半环表达式；需要额外的 fold/aggregation
  结构（NRC 的 summation、我们的 `Fold1/Fold0`、或带 aggregation 的
  provenance semiring）。U-semiring 的强项是 SPJ + union + distinct；
  聚合是扩展点。
- **NULL/3VL**：U-semiring 建模的是 multiplicity，不直接建模 SQL 的
  unknown；NULL 需要单独的 3VL 值层（Guagliardo–Libkin）。
- **顺序/窗口**：U-semiring 是 order-insensitive；`ORDER BY/LIMIT`/
  窗口需要 list/comonad 层。
- **递归**：U-semiring 不含 fixpoint；递归 CTE 需要 Datalog/不动点层。

> 因此 U-semiring 是"关系核 + 重复"的逻辑，不是"整个 SQL"的逻辑。
> 但它是目前最接近 SQL bag 语义的代数化逻辑。

---

## 4. NRC / 结构递归：纯 FP 的类型化演算

### 4.1 Nested Relational Calculus（NRC）

NRC 是带集合/嵌套集合的纯函数式查询演算：

```text
Types:  A ::= B | Unit | A × A | Set A | ...
Terms:  e ::= x | () | (e1,e2) | ∅ | {e} | e1 ∪ e2
           | { e | x ∈ e1 }        -- comprehension
           | map f e | filter p e
           | bigUnion e
           | srec ...               -- structural recursion
```

- **结构递归（structural recursion）** 是 NRC 的"fold"：
  对集合做归纳，保证 total（有限集合 → 有限集合）；
- 受限的结构递归 = 关系代数的表达力；
- 加 **summation/aggregation**、powerset 等可以表达更多（论文研究其
  内涵表达力）；
- NRC 有**类型系统**，是纯 FP；已有 Coq 形式化（查询语法与语义）。

### 4.2 为什么 NRC 对"更好表达 SQL 逻辑"重要

- 它把 **关系代数 + 嵌套 + 聚合** 放进一个 typed λ-calculus；
- `fold`/结构递归正好对应 SQL 的聚合（`foldMap`/`Monoid`），
  并且是 total 的；
- comprehension 正好对应 SQL 的 `SELECT-FROM-WHERE` 和嵌套子查询；
- 与 Haskell 的 `Foldable`/`Monad`/`Semigroup` 风格一致；
- 它为"查询语言的类型化 + 纯 FP + 聚合"提供了最直接的先例。

### 4.3 边界

- 原始 NRC 是 **集合语义**；SQL 是 bag。需要 multiset/bag 版
  （或在 semiring 里做）；
- NRC 不含 **顺序/窗口**（order-insensitive）；
- 不含 **NULL/3VL**（值是纯值）；需要单独的 option/3VL；
- 不含 SQL 的 **名字绑定/别名/相关子查询**（需扩展）；
- 不含 **递归**（除非加 fixpoint）；
- 等价性/优化在 NRC 层面有研究，但不如 U-semiring 工具成熟。

> NRC 是"纯 FP 类型化 SQL 逻辑"的最佳起点；U-semiring 是"bag/重复"的
> 最佳语义；两者结合 = multiset/semiring-valued NRC + folds。

---

## 5. Guagliardo–Libkin：真实 SQL 的参考语义

- **"A Formal Semantics of SQL Queries, Its Validation, and Applications"**
  （Guagliardo & Libkin, PVLDB 11(1):27–39, 2017）。
- 覆盖基本 SQL：`SELECT-FROM-WHERE` + 子查询 + set/bag 操作 + NULL；
  不回避真实语言的名字绑定、作用域、别名等；
- 用一个形式语义解释 SQL，并在大量随机查询/数据库上与真实引擎对照验证；
- 应用包括等价性、优化、语义验证。

**为什么重要：**

- 它证明"简化语义"（只讲 SPJ + 集合）会漏掉 SQL 的关键行为：
  NULL、名字绑定、相关子查询、bag/集合混合；
- 它给出了一个可作为 **参考语义** 的 ground truth；
- 对 Cagara：生成的 SQL 可以用它（或它的片段）作为语义基准，
  而不要只用"关系代数直觉"。

**局限：** 基本 SQL 片段，不含全部聚合/窗口/递归；等价性仍需工具/片段。

---

## 6. 其它关键理论

### 6.1 Monad comprehension

- `SELECT-FROM-WHERE` 是 monad comprehension：
  `{ e | x ← q, p }`；`JOIN` = bind + guard；`UNION ALL` = `MonadPlus`；
- 适合关系核和 surface（`do`/comprehension）；
- **不覆盖 GROUP BY**（group 不是 bind）；需要 fold 层。

### 6.2 Comonad / ordered

- `ORDER BY/LIMIT/窗口` 是 order-sensitive，不是关系算子；
- 用 `Ordered`/`NonEmpty`/list 或 stream comonad 建模；
- 窗口函数 = `duplicate`（邻域）+ `extract`（当前行）；
- 与 U-semiring/NRC 正交；等价性是 list 等价，不是 bag 等价。

### 6.3 3VL / NULL

- SQL 的 `NULL` 是 Kleene 三值逻辑；`NOT IN`/`EXISTS`/`NULL=NULL` 的语义
  不能只用 `Maybe` 近似；
- 设计选择：
  (a) 全 3VL（忠实但复杂）；
  (b) 值层 `Maybe` + 谓词层强制 2VL（Cagara 现在的方向），
      把 3VL 限制在边界；
- Guagliardo–Libkin 提供了 (a) 的参考语义；Cagara 的 (b) 更可组合，
  但要明确"与 SQL 的哪些查询等价"。

### 6.4 递归 / fixpoint

- 递归 CTE = 最小不动点；Datalog = 正 Horn + fixpoint；
- 递归查询的语义/等价性/终止性都是独立问题；
- 类型化 fixpoint（`Fix`/`Mu`）、分层否定（stratified negation）；
- 与关系核的 fold 不是一回事。

### 6.5 Provenance semiring

- 关系值 ∈ 任意交换半环（Bool/Nat/多项式/概率）；
- 统一 set/bag/provenance；聚合的 provenance 需要额外结构；
- 对调试/血缘/增量很有价值；
- 与 U-semiring 同源，但更一般。

### 6.6 Categorical / CQL

- schema 作为范畴、实例作为 functor、查询作为 Kan 扩张；
- 组合性极强，适合 schema/迁移/血缘；
- 聚合/顺序/窗口弱，不适合做 SQL 逻辑的唯一基础。

### 6.7 Graded / coeffect / refinement

- phase（row/agg/win）、cardinality（1:1/1:N）作为 grade；
- `NonEmpty`/`NonZero`/`Decimal p s`/`Nat` 作为精化；
- 类型正确性的关键补充，不是 SQL 逻辑本身。

---

## 7. 综合演算：SQL-λ

把上述理论组合成一个 typed calculus，作为"SQL 逻辑"的候选形式化。

### 7.1 Kinds / types

```text
Kinds:  Type, Row, Phase, Weight, Dialect
Types:
  A ::= Int | Bool | String | Date | Decimal p s | Maybe A
      | A × A | { l : A, ... } | Rel R r | Ord r | Group k r | Win r a
Rows:
  r ::= { l : A, ... } | r1 ∪ r2 | r \ l | ...
Weights:
  R ::= Nat | Bool | Provenance | ...
Phases:
  p ::= Row | Agg | Win
```

- `Rel R r`：`R`-weighted bag of row `r`；
- `Ord r`：有序序列（带排序见证）；
- `Group k r`：`k ↦ NonEmpty (Rel R r)`；
- `Win r a`：窗口/邻域；
- `Maybe A`：值层 NULL；
- `Decimal p s`：精度/尺度。

### 7.2 Terms

```text
q ::= table T
    | ∅
    | q1 ∪ q2
    | q1 ×_p q2
    | σ_p q
    | π_f q
    | distinct q
    | groupBy_k q
    | agg_{k,f} q
    | sortOn_k q
    | page_{n,m} q
    | window_s q
    | fix X. q                  -- optional, guarded/stratified
```

### 7.3 Typing（核心规则，示意）

```text
Γ ⊢ q : Rel R r       Γ ⊢ p : r → Bool
----------------------------------------
Γ ⊢ σ_p q : Rel R r

Γ ⊢ q1 : Rel R r      Γ ⊢ q2 : Rel R s
Γ ⊢ p : r → s → Bool
----------------------------------------
Γ ⊢ q1 ×_p q2 : Rel R (r × s)

Γ ⊢ q : Rel R r       Γ ⊢ f : r → s
----------------------------------------
Γ ⊢ π_f q : Rel R s

Γ ⊢ q : Rel R r
----------------
Γ ⊢ distinct q : Rel R r         -- 语义上应用 D

Γ ⊢ q : Rel R r       Γ ⊢ k : r → κ
----------------------------------------
Γ ⊢ groupBy_k q : Rel R (κ × Group r)

Γ ⊢ q : Rel R (κ × Group r)   Γ ⊢ f : r → m   Semigroup m
----------------------------------------------------------
Γ ⊢ agg_{k,f} q : Rel R (κ × m)

Γ ⊢ q : Rel R r       Γ ⊢ k : r → s   Ord s
--------------------------------------------
Γ ⊢ sortOn_k q : Ord r

Γ ⊢ q : Ord r         n,m : Nat
-------------------------------
Γ ⊢ page_{n,m} q : Ord r
```

要点：

- `σ` 的谓词是 `Bool`（2VL），不是 `Maybe Bool`；3VL 被限制在边界；
- `groupBy` 的内层是 `NonEmpty`，所以 `agg` 用 `Semigroup`，
  空组不存在 → 非空聚合无 NULL；
- `page` 要求 `Ord`；未排序分页类型错误；
- `distinct` 改变语义（D），但类型保持 `Rel R r`（或把 `R` 标成
  "支持 D" 的半环）；
- `window` 是 `Ord`/`Win` 上的 comonadic 操作。

### 7.4 语义（概念版）

```text
⟦Rel R r⟧         = { f : r → R | f 有限支撑 }
⟦∅⟧               = 0
⟦q1 ∪ q2⟧         = ⟦q1⟧ + ⟦q2⟧
⟦q1 ×_p q2⟧(r,s)  = Σ_{r,s} ⟦q1⟧(r) × ⟦q2⟧(s) × [p(r,s)]
⟦σ_p q⟧(r)        = ⟦q⟧(r) × [p(r)]
⟦π_f q⟧(s)        = Σ_{r: f(r)=s} ⟦q⟧(r)
⟦distinct q⟧(r)   = D(⟦q⟧(r))
⟦groupBy_k q⟧     = 按 k 分组，内层非空，权重按 R 求和
⟦agg_{k,f} q⟧     = 对每组做 foldMap1 f
⟦sortOn_k q⟧      = 按 k 排序的有序序列
⟦page_{n,m} q⟧    = drop n ∘ take m
⟦window_s q⟧      = 由 s 决定的 comonadic 邻域变换
```

- `Maybe` 值层：`⟦Maybe A⟧ = A + 1`；
- 谓词 `Bool`：SQL 的 3VL 在边界处被消解（或显式 `whereTrue`）；
- 若要完整 SQL 语义，可把值域换成 3VL/Kleene，谓词层用 3VL；
  Guagliardo–Libkin 是参考。

### 7.5 定律

U-semiring 层：

```text
q + 0 = q
q1 + q2 = q2 + q1
(q1 + q2) + q3 = q1 + (q2 + q3)
(q1 × q2) × q3 = q1 × (q2 × q3)
q × 1 = q
q1 × (q2 + q3) = q1×q2 + q1×q3
a × (Σ_i b_i) = Σ_i (a × b_i)
D(D(q)) = D(q)        -- 幂等
-- D 与 +/× 的支持律见 U-semiring 论文
```

fold 层：

```text
fold1 f (map g q) = fold1 (f ∘ g) q
groupBy_k (σ_p q) = regroup (map (second (σ_p)) (groupBy_k q))
agg_{k,f} (σ_{k∘p} q) = σ_{p∘fst} (agg_{k,f} q)   -- 只按 key 过滤
```

ordered/comonad 层：

```text
sortOn k (sortOn k q) = sortOn k q
page n m (page n' m' q) = ...        -- 明确 drop/take 顺序
extract ∘ duplicate = id             -- comonad
```

Maybe/值层：

```text
coalesce d (just x) = x
eqMaybe ...                          -- SQL NULL 的显式比较
```

### 7.6 规范形与 SQL 编译

U-expression 规范形（示意）：

```text
D( Σ_i ∏_j q_{ij} )                 -- 和之积 + duplicate elimination
```

- 每个 `∏_j q_{ij}` 对应一个 `FROM/JOIN`；
- `Σ_i` 对应 `UNION ALL`；
- `D` 对应 `DISTINCT`/`UNION`；
- `groupBy`/`agg` 是作用在 `Σ` 上的 fold；
- `sortOn`/`page` 是外层 presentation；
- 共享子表达式 → CTE；
- SQL 的"形状"（HAVING vs 派生表、CTE vs 内联、join 顺序）是
  同一规范形的不同 presentation，用代价/启发式选择。

---

## 8. 等价性：哪些能证，怎么证

### 8.1 片段与已知结果

| 片段 | 等价性/包含性 | 备注 |
|---|---|---|
| CQ（集合语义） | containment NP-complete | Chandra–Merlin 1977 |
| CQ（bag 语义，带不等式） | 可判定于 PSPACE；至少和图同构一样难；精确复杂度开放 | Green 等 |
| SPJ + union + distinct（bag） | U-semiring/U-expression；UDP/SMT；Cosette/QED | 实际工具可证很多规则 |
| + 聚合/HAVING | 无完整判定过程；需 fold 律 + 差分 + 研究 | 开放 |
| + ORDER/LIMIT/窗口 | 不是 bag 等价；list/comonad 语义 | 顺序敏感，工具弱 |
| + NULL/3VL | 参考语义 Guagliardo–Libkin；等价性研究型 | 复杂 |
| + 递归 CTE | 一般不可判定；Datalog 片段可判定 | 受限片段 |
| 完整 SQL | 不可判定 | 不要承诺 |

### 8.2 证明/验证工具

- **UDP**（U-expression decision procedure）与 **U-semiring** 公理：
  把查询转成 U-expression，规范化后用 SMT 判定；
- **Cosette**（CIDR 2017）：SQL 等价性证明/反例，支持约束/键；
- **QED**（PVLDB 17, 2024）：U-semiring + SMT 的实用决策器；
- **SQLSolver**（SIGMOD 2024）：线性整数算术的等价性证明；
- **SPES**（2020）：bag 语义下的符号等价性；
- **Differential testing**：SQLite/DuckDB/Postgres 等引擎上的随机数据差分；
- **参考解释器**：按 SQL-λ 语义实现 `⟦q⟧`，做 property-based 差分；
- **Coq/Lean 机械化**：NRC 已有 Coq 形式化；U-semiring/编译正确性可机械化。

### 8.3 对 Cagara 的策略

1. 关系核（SPJ + union + distinct）用 U-expression 规范形 + U-semiring 律；
   能用 SMT/UDP 证的就证，不能的用差分。
2. 聚合用 fold 律（group 非空、fold fusion）+ 参考解释器差分；
   不要承诺"完整聚合等价"。
3. ORDER/LIMIT/窗口用 list/comonad 律 + 顺序敏感测试。
4. NULL/3VL 用参考语义（Guagliardo–Libkin 片段）或明确 Cagara 的
   "2VL 边界"语义；不要混淆。
5. 递归只支持受限片段（stratified/guarded），并明确不保证一般等价。
6. 所有重写规则都配 **property test + 引擎差分**；关键规则用 Cosette/QED
   （能覆盖的片段）做交叉验证。

---

## 9. 这对 Cagara 意味着什么

### 9.1 语义 IR

- 把 `Rel` 从"bag"升级成 **`Rel R r`**（权重半环 `R`）：
  - bag = `Nat`，set = `Bool`，provenance = 多项式；
  - `UNION ALL` = `+`，`JOIN` = `×`，`WHERE` = 指示函数乘，
    `SELECT` = 求和，`DISTINCT` = `D`；
- 这让 bag/重复/投影求和有统一的代数语义，也让"逻辑等价"有基础；
- 聚合仍用 `Group`/`Fold1`/`Fold0`（不用半环硬编码）；
- 顺序/窗口用 `Ord`/`Win`/comonad；
- NULL 用 `Maybe` + 2VL 谓词边界（或显式 3VL）。

### 9.2 类型系统

- row types + `Disjoint`/`Merge`（join 冲突类型错误）；
- phase grade（Row/Agg/Win）+ `Combine` 类型族；
- `NonEmpty`/`Semigroup` 聚合；`Nat` 分页；`Decimal p s`；`newtype`/ADT；
- dialect capability（`Supports d Feature`）；
- `newtype`/ADT + `SqlRep`；封闭 type classes；
- total（`safeDiv`/`Maybe`；无 `undefined`/异常/静默溢出）。

### 9.3 Surface

- comprehension/`do` + `groupBy`/`foldMap` + `sortOn`/`take`；
- Haskell 的 `&`/`$`/`>>>`/`let`/`case` 保留；
- `select/update` 让位给 `fmap`/record update；
- `<>` 是 Semigroup；`/=` 是不等；
- 符号简写若保留，只作为 lawful 算子的 sugar。

### 9.4 研究优先级

1. **U-expression as IR**：SPJ + UNION + DISTINCT 片段
   先做；验证 bag/duplicate 律；接 Cosette/QED。
2. **Fold 层**：`Group`/`NonEmpty`/`Fold1`/`Fold0`；聚合律 + 差分。
3. **Order/comonad 层**：`Ord`/`Win`；分页/窗口语义。
4. **3VL/NULL**：参考 Guagliardo–Libkin；明确 Cagara 语义边界。
5. **类型层**：row/phase/dialect/refinement；free monad 归一化。
6. **机械化/等价性**：Coq/NRC/U-semiring 片段；SMT/UDP；差分。
7. **递归**：受限 fixpoint/stratified Datalog；开放。
8. **provenance/cardinality**：可选，后续。

---

## 10. 开放问题（研究议程）

1. **U-semiring + folds**：如何把任意 monoid fold/聚合纳入 U-expression，
   同时保持可判定/可证明的等价性片段？GROUP BY/HAVING 的代数公理是什么？
2. **U-semiring + 3VL**：multiplicity 与 SQL NULL 的联合语义；
   `DISTINCT`/`GROUP BY`/`IN`/`NOT IN` 的 NULL 交互；等价性。
3. **U-semiring + order/windows**：bag 语义 + 有序呈现的组合；
   窗口帧的等价性；`ORDER BY` 与 `DISTINCT`/聚合的交互。
4. **Typed calculus**：SQL-λ 的类型可靠性（progress/preservation）、
   grade 系统、free monad 归一化的类型保持。
5. **机械化**：Coq/Lean 里形式化 SQL-λ + U-semiring + NRC；
   证明重写律/编译正确性；与 QED/Cosette 交叉验证。
6. **复杂度**：带聚合/order/window 的片段的可判定性；
   bag CQ 的精确复杂度；bounded equivalence/反例生成。
7. **编译正确性**：SQL-λ → SQL 的语义保持；one-SQL 定理；
   动态/数据依赖查询的拒绝或 query shredding。
8. **cardinality/provenance**：graded semiring、fan-out、聚合 provenance、
   增量（Datafun）。
9. **类型正确性**：`NonEmpty`/`NonZero`/`Decimal`/`Nat` 的精化系统；
   与 SQL 引擎约束（CHECK/primary key）的对应；schema 漂移。
10. **方言/引擎**：类型级 dialect capability；引擎语义差异（除零/溢出/
    NULL 排序/十进制）的形式化；跨引擎等价性。
11. **Surface/人体工学**：comprehension/do 的类型；错误信息；
    与 Haskell mind model 的一致性。
12. **性能**：U-expression 归一化的爆炸；e-graph/equality saturation；
    代价模型。

---

## 11. 参考文献

### 语义/逻辑

1. **Codd, "A Relational Model of Data for Large Shared Data Banks"**（1970）——
   关系模型。
2. **Chandra & Merlin, "Optimal Implementation of Conjunctive Queries in
   Relational Data Bases"**（STOC 1977）——集合语义 CQ containment NP-complete。
3. **Green 等, "Bag Equivalence of Bounded Symmetry-Degree Conjunctive Queries
   with Inequalities"**（AMW 2011）——bag 语义 CQ 等价性：PSPACE 可判定、
   graph-isomorphism-hard。
4. **Guagliardo & Libkin, "A Formal Semantics of SQL Queries, Its Validation,
   and Applications"**（PVLDB 11(1):27–39, 2017）——真实 SQL 的形式语义
   （bag/NULL/名字绑定），实验验证。
5. **Chu 等, "Axiomatic Foundations and Algorithms for Deciding Semantic
   Equivalences of SQL Queries"**（PVLDB 11(10), 2018）——U-semiring/UDP。
   [PDF](https://www.vldb.org/pvldb/vol11/p1482-chu.pdf)
6. **Cosette**（Chu, Wang, Weikum, Suciu, CIDR 2017）——SQL 等价性证明器。
   [PDF](https://www.cidrdb.org/cidr2017/papers/p51-chu-cidr17.pdf)
7. **QED: A Powerful Query Equivalence Decider for SQL**（PVLDB 17, 2024）——
   U-semiring + SMT 的实用决策器。
   [PDF](https://www.vldb.org/pvldb/vol17/p3602-wang.pdf)
8. **SQLSolver: Proving Query Equivalence Using Linear Integer Arithmetic**
   （SIGMOD 2024）——LIA 等价性证明。
9. **SPES: A Symbolic Approach to Proving Query Equivalence Under Bag
   Semantics**（2020）——bag 语义符号证明。
10. **Green, Karvounarakis, Tannen, "Provenance Semirings"**（PODS 2007）——
    半环语义/provenance。

### 类型化查询演算

11. **NRC / 结构递归**：Buneman、Wong、Tannen 一系；
    "Structural Recursion as a Query Language"、"Querying Nested Collections"
    （Wong 博士论文）；NRC 的 Coq 形式化。
12. **Grust, "Monad Comprehensions: A Versatile Representation for Queries"**
    （2004）——comprehension → SQL。
    [PDF](https://db.in.tum.de/~grust/files/monad-comprehensions.pdf)
13. **Cheney, Lindley, Wadler, "A Practical Theory of Language-Integrated
    Query"**（ICFP 2013）——quotation + normalization + one-SQL。
    [PDF](https://homepages.inf.ed.ac.uk/slindley/papers/practical-theory-of-linq.pdf)
14. **Links / Ferry / HaskellDB**——typed SQL embedding 的工程先例。
    [Ferry](https://db.cs.uni-tuebingen.de/research/past-projects/ferry/)
15. **Uustalu & Vene, "The Essence of Dataflow Programming"**（2005）——
    comonad/窗口。
16. **Orchard 等, graded monads；Petricek 等, coeffects**——grade/phase/基数。
17. **Liquid Haskell**——精化类型（`NonEmpty`/`NonZero`/`Nat`）。
18. **Hughes, "Generalising Monads to Arrows"**（2000）——静态形状。
19. **Kiselyov 等, "Finally tagless"；Swierstra, "Data types à la carte"**——
    多解释器/可扩展 AST。

### 递归/不动点

20. **Abiteboul, Hull, Vianu, "Foundations of Databases"**（1995）——
    关系代数、Datalog、不动点、复杂度。
21. **Datafun**（Arntzenius & Krishnaswami, 2016）——增量/单调查询（可选）。

### 范畴/语义

22. **Spivak, "Functorial Data Migration"**（2012）；**CQL**——
    schema/血缘/迁移的语义参考（不作为 SQL 逻辑的唯一基础）。

---

## 12. 一句话总结

> SQL 的逻辑 = **semiring-valued 正存在逻辑 + unbounded summation +
> duplicate elimination + monoid folds + ordered/comonadic presentation +
> 3VL boundary + fixpoint**。
>
> U-semiring 给出 bag/重复/投影求和的代数逻辑；
> NRC/结构递归给出纯 FP 类型化的关系+聚合演算；
> Guagliardo–Libkin 给出真实 SQL 的 NULL/名字绑定参考语义；
> comonad 给出顺序/窗口；Datalog 给出递归；精化/graded 给出类型正确。
>
> 把它们组合成 **SQL-λ**，用 U-expression 规范形 + fold/comonad 律 +
> SMT/差分测试保证等价性，才是"更好表达 SQL 逻辑"的可实现路径。