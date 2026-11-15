# SQL-λ：Cagara 核心演算规格（草案 v0.1）

> 这是"大方面"研究的第一步：**只定义核心演算**，不写实现、不做形式化证明。
> 目标是把语义分层、类型结构、表示方式、舞台边界、等价性这几件事一次性钉住。
>
> 范围：静态片段 `SPJ + UNION ALL/UNION/DISTINCT + GROUP BY/聚合/HAVING +
> ORDER BY/LIMIT/OFFSET`，以及为此服务的行类型、phase、Maybe、参数、方言约束。
> 窗口/外连接/递归等作为**扩展**列出，不塞进核心。

---

## 0. 设计原则

1. **SQL 是解释器，不是语法模板。** 核心 term 有自己的语义；SQL 是其中一个后端。
2. **四层不可合并**：关系（bag/semiring）、折叠（monoid）、呈现（order/comonad）、
   值（option）。跨层必须换类型。
3. **静态形状**：查询的结构不能依赖行数据；`do`/initial algebra 记录结构，
   归一化负责生成一条 SQL 或拒绝。
4. **类型即边界**：行、phase、参数、方言、精度、非空/非零都进类型；
   没有运行期"缺列/嵌套/丢失参数/未验证方言"。
5. **无糖**：核心只有 `pure`/`>>=`/`>>>`/`map`/`filter`/`unionAll`/`distinct`/
   `groupBy`/`fold1`/`orderBy`/`page`/`field`/records/`if`/`let`；
   不设 `summarize`/`window`/`select`/`update`/符号简写。
6. **total**：无 `undefined`/异常/静默溢出；偏函数返回 `Maybe`/`NonEmpty`。

---

## 1. Kinds

```text
K ::= Type | Row | Phase | Card | Dialect | Weight
```

- `Phase`：`Row | Agg | Win`；
- `Card`：`One | Many`（可选扩展；本文核心先用 `Rel`，cardinality 作为扩展）；
- `Weight`：`Bag | Bool | Prov`（可选；核心先用 bag）；
- `Dialect`：目标方言（后端约束，不在核心语义里）。

---

## 2. Types

```text
A, B ::= Int | Bool | String | Date | Timestamp
       | Decimal (p :: Nat) (s :: Nat)
       | Maybe A
       | A × B
       | { l : A, ... }              -- record
       | Expr (g :: Phase) r A
       | Query r s
       | Group r
       | Ordered r
       | Param A
       | Code (Expr g r A)

Rows:
  r, s ::= { l : A, ... } | r1 ∪ r2 | r \ l | ρ
```

派生：

```text
Rel r       = Query () r              -- 闭合关系（bag of r）
Expr r A    = Expr Row r A            -- 行阶段表达式
-- Group r 是内部非空 bag；查询输出行写成 (k, Group r)
```

- `Expr g r A`：行环境 `r` 上的表达式，phase `g`；
- `Query r s`：从输入行 `r` 到输出行 `s` 的**索引关系**；
- `Group r`：**非空** bag of `r`（组的内部）；
- `Ordered r`：有序呈现（排序见证）；
- `Param A`：宿主/编译期参数；
- `Code (Expr ...)`：quoted expression（参数/反引用）。

---

## 3. Terms

### 3.1 关系层

```text
q ::= table T                       -- Table r -> Query () r
    | pure                         -- Query r r（恒等关系）
    | q >>= k                      -- Query r s -> (s -> Query s t) -> Query r t
    | q1 >>> q2                    -- Query r s -> Query s t -> Query r t（Category）
    | map e                        -- Expr Row r s -> Query r s
    | filter p                     -- Expr Row r Bool -> Query r r
    | unionAll q1 q2               -- Query r s -> Query r s -> Query r s
    | distinct q                   -- Query r s -> Query r s
    | groupBy key                  -- Expr Row r k -> Query r (k, Group r)
    | fold1 f                      -- Fold1 r m -> Query r (k, Group r) -> Query r (k, m)
    | orderBy keys                 -- [Expr Row r key] -> Query () r -> Ordered r
    | page skip take               -- Nat -> Nat -> Ordered r -> Ordered r
    | window spec                  -- 扩展：WinSpec -> Query r r -> Query r (r, w)
```

### 3.2 折叠层

```text
Fold1 r m ::= 对非空 bag r 做 foldMap1 的聚合值
sum, count, avg, min, max, countDistinct, sumIf, countIf, ...
```

- `Fold1 r m` 是**第一类值**（不是语法）；
- `fold1 f g` 对每个非空组做 `f`；
- `Group r` 非空，所以 `sum/min/max` 不需要 `Maybe`；
- `fold0`（可能空的 fold）只用于窗口帧/空集；返回 `Maybe m`。

### 3.3 呈现层

```text
Ordered r       -- 有序的行序列（list + sorted witness）
orderBy keys    -- 生成 Ordered
page skip take  -- Ordered 上的单一分页算子（没有 limit/offset 两段）
```

### 3.4 值层

```text
just, nothing, coalesce, isNull, isNotNull, eqMaybe
ifThenElse, case, let, records, field
Num/Fractional/Semigroup/Monoid/IsString 实例；lifted Eq/Ord/Bool
```

### 3.5 舞台

```text
Param A
~p                              -- antiquote：把 Param 插入表达式
Code (Expr g r A)               -- quoted expression
```

### 3.6 扩展（不在核心）

```text
leftJoin / rightJoin / fullJoin -- Maybe 行
semiJoin / antiJoin             -- EXISTS/NOT EXISTS
window spec                     -- 窗口
intersect / except              -- 集合层
groupingSets / rollup           -- 聚合形状
fix                             -- 递归
```

---

## 4. Typing

### 4.1 表达式

```text
Γ, l : A ∈ r
---------------------------  (Field)
Γ ⊢ field @l : Expr Row r A

Γ ⊢ e : Expr g r A       A 有 Num 实例
----------------------------------------  (Num)
Γ ⊢ e + e' : Expr g r A

Γ ⊢ p : Expr g r Bool
Γ ⊢ a : Expr g r A
Γ ⊢ b : Expr g r A
---------------------------  (If)
Γ ⊢ if p then a else b : Expr g r A

Γ ⊢ x : Expr g r A      Γ ⊢ y : Expr g r A
-------------------------------------------  (LiftEq)
Γ ⊢ x .== y : Expr g r Bool

Γ ⊢ p : Expr g r Bool    Γ ⊢ q : Expr g r Bool
----------------------------------------------  (LiftBool)
Γ ⊢ p .&& q : Expr g r Bool

Γ ⊢ e : Expr Row r A
----------------------  (Group/agg 标记，见下)
...
```

- `Num`/`Fractional`/`Semigroup`/`Monoid`/`IsString` 的 lift 是实例；
- `Eq`/`Ord`/`Bool` 是 lifted 类（不能复用 Haskell `Eq`，因为结果类型是
  `Expr r Bool`）；
- `field` 是 `HasField` 投影；`local` 是 row map（类型级）；
- 表达式没有"特制 AST 节点"，只有 `Field`/`Prim`/`App`/`If`/`Local`
  这些 primitive。

### 4.2 查询（indexed monad）

```text
-------------------  (Pure)
Γ ⊢ pure : Query r r

Γ ⊢ q : Query r s      Γ, x : s ⊢ k : Query s t
-----------------------------------------------  (Bind)
Γ ⊢ q >>= k : Query r t

Γ ⊢ q1 : Query r s      Γ ⊢ q2 : Query s t
-------------------------------------------  (Compose)
Γ ⊢ q1 >>> q2 : Query r t

Γ ⊢ e : Expr Row r s
--------------------  (Map)
Γ ⊢ map e : Query r s

Γ ⊢ p : Expr Row r Bool
-----------------------  (Filter)
Γ ⊢ filter p : Query r r

Γ ⊢ q1 : Query r s      Γ ⊢ q2 : Query r s
-------------------------------------------  (UnionAll)
Γ ⊢ unionAll q1 q2 : Query r s

Γ ⊢ q : Query r s
------------------  (Distinct)
Γ ⊢ distinct q : Query r s
```

- `Query r s` 的语义是 `r -> Bag s`；`pure r = {r}`；
  `(q >>= k) r = ⋃_{s ∈ q r} k s s`；
- `map e`、`filter p` 是关系变换；
- `distinct` 在语义上应用 duplicate elimination；类型保持 `Query r s`。

### 4.3 分组/聚合

```text
Γ ⊢ key : Expr Row r k
-----------------------
Γ ⊢ groupBy key : Query r (k, Group r)

Γ ⊢ f : Fold1 r m
-------------------------------------------
Γ ⊢ fold1 f : Query (k, Group r) (k, m)

Γ ⊢ q : Query r (k, Group r)     Γ ⊢ fold1 f as above
----------------------------------------------------
Γ ⊢ q >>> fold1 f : Query r (k, m)
```

- `Group r` 非空；
- `sum`/`count`/`avg`/`min`/`max` 是 `Fold1 r m`；
- `avg` 用 `(Sum, Count)` 半群，组非空 → 非空；
- `HAVING` = `filter` 之后（语义上对聚合结果过滤）。
- 聚合值的类型（示意）：

```text
sum   : Expr Row r a -> Fold1 r (Sum a)
count : Fold1 r Count
avg   : Expr Row r a -> Fold1 r (Avg a)
min   : Expr Row r a -> Fold1 r a
max   : Expr Row r a -> Fold1 r a
```

### 4.4 呈现

```text
Γ ⊢ keys : [Expr Row r key]     Γ ⊢ q : Query () r
---------------------------------------------------
Γ ⊢ orderBy keys q : Ordered r

Γ ⊢ o : Ordered r     Γ ⊢ skip : Nat     Γ ⊢ take : Nat
-------------------------------------------------------
Γ ⊢ page skip take o : Ordered r
```

- `Ordered` 只能由 `orderBy` 产生；`page` 只能作用在 `Ordered` 上；
- 没有"未排序分页"；没有"先 limit 再 offset"。

### 4.5 值/Maybe

```text
Γ ⊢ x : Expr g r A
--------------------  (Just)
Γ ⊢ just x : Expr g r (Maybe A)

Γ ⊢ d : Expr g r A     Γ ⊢ m : Expr g r (Maybe A)
-------------------------------------------------
Γ ⊢ coalesce d m : Expr g r A

Γ ⊢ m : Expr g r (Maybe A)
--------------------------
Γ ⊢ isNull m : Expr g r Bool
```

- 谓词是 `Expr g r Bool`；`Maybe Bool` 用 `whereTrue`/`isTrue` 显式降级；
- `sum`/`count` 对 `Maybe` 输入用 `Fold0`/`Maybe` 目标；
- `Ord` 对 `Maybe` 不用 Haskell 的 `Nothing < Just`；用显式 `NullsOrder`。

### 4.6 参数

```text
Γ, p : Param A ⊢ e : Expr g r B
--------------------------------
Γ ⊢ λp. e : Param A -> Expr g r B

Γ ⊢ p : Param A
----------------  (Antiquote)
Γ ⊢ ~p : Expr g r A
```

- `Param` 与 `Row` 是两个环境；`~p` 只插入 `Param`；
- 归一化后是 SQL 参数/字面量；静态片段内一条 SQL。

---

## 5. Semantics（denotational，简述）

```text
⟦Query r s⟧ : r -> Bag s
⟦pure⟧ r = {r}
⟦q >>= k⟧ r = ⋃_{s ∈ ⟦q⟧ r} ⟦k s⟧ s
⟦map e⟧ r = {⟦e⟧ r}
⟦filter p⟧ r = if ⟦p⟧ r then {r} else ∅
⟦unionAll q1 q2⟧ r = ⟦q1⟧ r ⊎ ⟦q2⟧ r
⟦distinct q⟧ r = D(⟦q⟧ r)
⟦groupBy key⟧ r = groupBy(⟦key⟧) (⟦q⟧ r)
⟦fold1 f⟧ (k, g) = (k, foldMap1 f g)
⟦orderBy keys⟧ = sortBy ...
⟦page skip take⟧ = drop skip ∘ take take
```

- `Bag` 是有限多重集；`D` 是 duplicate elimination；
- `Expr Row r A` 的语义是 `r -> A`；
- `Expr Agg/Win` 是 typing-only；聚合/窗口的语义由 fold/comonad 给出；
- `Maybe` 值是 `A + 1`；`Group` 是 `NonEmpty (Bag r)`；
- `Ordered` 是带排序见证的 list；`Param` 在编译期替换。

**片段承诺：**

- 语义覆盖 `SPJ + UNION ALL + DISTINCT + GROUP BY/聚合 + ORDER/LIMIT/窗口`；
- 不含递归、任意相关子查询、`LATERAL`、DDL/DML；
- NULL 用 `Maybe` + 2VL 谓词边界；完整 3VL 是扩展；
- 方言语义由 capability 约束；核心语义是方言无关的。

---

## 6. Laws（10 条）

### L1. Query indexed monad

```text
pure >>= k        = k
q >>= pure        = q
(q >>= k) >>= h   = q >>= (k >=> h)
(q1 >>> q2) >>> q3 = q1 >>> (q2 >>> q3)
pure >>> q = q       q >>> pure = q
```

### L2. UnionAll 交换幺半群

```text
q ∪ ∅ = q
q1 ∪ q2 = q2 ∪ q1
(q1 ∪ q2) ∪ q3 = q1 ∪ (q2 ∪ q3)
```

### L3. map/filter 分配

```text
(q1 ∪ q2) >>> map e = (q1 >>> map e) ∪ (q2 >>> map e)
(q1 ∪ q2) >>> filter p = (q1 >>> filter p) ∪ (q2 >>> filter p)
q >>> map e >>> filter p = q >>> filter p >>> map e   -- p 不依赖 e 时
```

### L4. Distinct

```text
distinct (distinct q) = distinct q
distinct (q1 ∪ q2) = distinct (distinct q1 ∪ distinct q2)
```

（`distinct` 不是半环同态；只给支持律。）

### L5. Fold fusion

```text
foldMap1 f (fmap g xs) = foldMap1 (f ∘ g) xs         -- xs : NonEmpty (Bag r)
fold1 f (q >>> groupBy k) = ...                     -- 对每个非空组 fold
```

### L6. 聚合半群

```text
sum (x <> y) = sum x <> sum y            -- 目标 m 的半群
count = foldMap1 (const (Count 1))
avg = (Sum, Count) 半群后相除
```

### L7. Order/Page

```text
orderBy keys q 的排序是稳定的（同 keys 的再次排序不改变行序）
page s t 的合成律要明确 skip/take 顺序
page 只能在 Ordered 上
```

### L8. Maybe

```text
coalesce d (just x) = x
isNull nothing = true
eqMaybe (just x) (just y) = x .== y
eqMaybe nothing nothing = true
```

### L9. 参数

```text
(λp. e) a = e[a/p]
~p 的替换是 capture-avoiding
[静态片段] 归一化后一条 SQL
```

### L10. 类型类

```text
Functor/Applicative/Monad 律
Num/Fractional/Semigroup/Monoid/IsString 逐点律
LiftEq/LiftOrd/LiftBool 与基础类一致
```

> 以上是**可检验的等式**，不是形式化证明；Step 2/3 用 property test +
> 引擎差分 + compile-fail test 验证。

---

## 7. Normal form 与 SQL 编译

### 7.1 U-expression

```text
D( Σ_i ∏_j q_ij )
```

- `q_ij` 是表/指示函数/join 条件；
- `Σ` 对应 `UNION ALL`；
- `∏` 对应 `JOIN`/笛卡尔积 + 条件；
- `D` 对应 `DISTINCT`/`UNION`；
- `groupBy`/`fold1` 是作用在 `Σ` 上的 fold；
- `orderBy`/`page` 是外层 presentation；
- 共享子表达式 → CTE。

### 7.2 SQL 形状

- `HAVING` vs 派生表：同一 core term，两种等价 SQL；
- `CTE` vs 内联：共享/性能；
- join 顺序：半环交换律 + 代价；
- `DISTINCT ON` vs 窗口：方言/语义；
- 窗口子查询 vs `QUALIFY`（方言支持时）。

**形状选择不是语义**；所有形状由 core term 的语义决定，等价性由 §6 的律保证。

### 7.3 方言

```haskell
class Supports (d :: Dialect) (f :: Feature)
compile :: forall d. KnownDialect d => Query () r -> SQL d
```

- `FullJoin`/`NullsFirst`/`Decimal`/`TryCast` 等 capability；
- 未支持 = 类型错误；不承诺"能生成就能运行"。

---

## 8. Surface / elaboration（无糖）

**核心 surface：**

```haskell
-- 关系（Query 用 >>> 组合，或用 >>= 做依赖 bind）
q >>> map e
q >>> filter p
q1 `unionAll` q2
q >>> distinct

-- 聚合（groupBy/fold1 是 Query 变换）
q >>> groupBy key >>> fold1 sum
q >>> groupBy key >>> fold1 (record aggregate)   -- 记录聚合
q >>> groupBy key >>> fold1 sum >>> filter having

-- 呈现（orderBy/page 是函数，返回 Ordered）
q & orderBy keys & page skip take

-- 值
field @l / records / if / let / just / coalesce

-- 参数
\p -> e
~p
```

- 没有 `summarize`/`window`/`select`/`update`/`agg` stage 语法；
- 没有 `&?`/`&=`/`&+`/`&*` 符号简写；
- `do`/`fmap`/`filter`/`groupBy`/`fold1`/`orderBy`/`page` 都是普通函数/primitive；
- `groupBy`/`fold1` 可以用记录组合成多聚合；
- 窗口/外连接/递归作为扩展函数/类型，不进核心语法。

**elaboration：** surface → core term；不改变语义；不做类型驱动的隐式重写
（除了类型类实例/默认化，这是 overloading，不是 sugar）。

---

## 9. 参数舞台

```text
Param A        -- 宿主/编译期值
Expr g r A     -- 行表达式
Code (Expr g r A)
```

- 两个环境：`Param` 与 `Row`；不要混成一个 Reader；
- `~p` 反引用；归一化替换；capture-avoiding；
- 静态片段内 "one SQL"；动态查询（依赖行选择结构）拒绝/多 SQL 是扩展；
- 这修掉两个真实 bug：参数经 projection 丢失、同一参数两次 join 丢一侧。

---

## 10. Maybe / totality

- `Maybe` 在值层；`just`/`nothing`/`coalesce`/`isNull`/`eqMaybe`；
- 谓词 `Bool`；`Maybe Bool` 用 `whereTrue`；
- `Group` 非空 → `fold1` 非空；窗口帧可能空 → `fold0`/`Maybe`；
- `NonEmpty`/`NonZero`/`Nat`/`Decimal p s` 作为精化/类型；
- 无 `undefined`/异常/静默溢出；`safeDiv`/checked arithmetic；
- 归一化 fuel/终止度量；递归是独立扩展。

---

## 11. 扩展（不进核心）

| 扩展 | 类型/语义 | 备注 |
|---|---|---|
| `leftJoin`/`rightJoin`/`fullJoin` | 右/左行 `Maybe`；外连接语义 | Null 扩展；`Maybe` 行 |
| `semiJoin`/`antiJoin` | 保留左行；EXISTS/NOT EXISTS | 相关子查询片段 |
| `window` | WinSpec + comonad/帧 | 独立 phase；不能嵌套 |
| `intersect`/`except` | 集合层商 | 与 bag/union 分开 |
| `groupingSets`/`rollup` | 多个 groupBy + union + `Nothing` key | 类型化组合子 |
| `fix` | 最小不动点 | 递归 CTE；终止/单调 |
| `LATERAL`/任意相关子查询 | 超出核心 | escape hatch/多 SQL |

---

## 12. 明确不做

- DDL/DML/事务；
- 用户自定义函数/类型（先封闭）；
- 全依赖类型；
- 全 SQL 等价承诺；
- 把 `.field`/surface 小技巧放进核心；
- 动态查询结构（依赖行数据选择表/查询）；
- 未验证方言的静默支持；
- 隐式 cast/隐式默认化（类型类默认化除外）。

---

## 13. 待拍板（大决定）

1. **`Query r s` vs `Rel r`**：本规格选 `Query r s`（索引 monad），
   `Rel r = Query () r`；需要确认 `do`/`pure` 规则和实现成本。
2. **Deep vs Tagless**：核心选 deep initial algebra；tagless 只做库/多解释器。
3. **权半环**：核心先用 `Bag`；`Rel w r`/provenance 作为扩展。
4. **聚合类型**：`Fold1 r m` 第一类值 + 记录聚合；是否需要更精确的依赖记录。
5. **NULL**：`Maybe` + 2VL 边界，还是完整 3VL；先 2VL。
6. **窗口/外连接**：扩展还是核心；先扩展。
7. **`GROUPING SETS`**：后续；先多个 groupBy + union。
8. **方言**：核心语义方言无关；capability 在后端。
9. **schema**：类型级 `Table` 由 DB/DDL 生成；核心不定义内省。
10. **surface**：无糖核心是否够用；`.field`/records/`do` 的最终形态后置。

---

## 14. 下一步（Step 2）

做**最小类型检查原型**，不实现 SQL 后端：

- 实现 `Row`/`HasField`/`Expr g r a`/`Query r s`/`Group`/`Ordered`；
- 实现 `pure`/`>>=`/`map`/`filter`/`unionAll`/`distinct`/`groupBy`/`fold1`/
  `orderBy`/`page`；
- 实现 `Maybe`、`Param`、lifted `Eq/Ord/Bool` 的最小集合；
- 用 10 个例子：inner join、groupBy+sum/count/avg、HAVING、leftJoin（扩展）、
  window（扩展）、top-N、set ops、params、null、date/decimal；
- 验证：类型规则是否自洽、错误信息是否可读、`Query r s` 是否比 `Rel r` 好写。

Step 3 再做归一化/等价性（U-expression + 律 + SQL 形状 + 差分）。

---

## 15. 一句话

> **SQL-λ 的核心 = 索引关系 `Query r s`（bag/indexed monad）+
> 行阶段 `Expr g r a`（reader/grade）+ 非空组 fold + 有序呈现 +
> 值层 `Maybe` + 两级参数 + 方言 capability；
> SQL 是 `Query () r` 的一个解释器，不是核心语法。**