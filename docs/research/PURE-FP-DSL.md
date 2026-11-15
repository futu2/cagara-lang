# 面向 Haskell Mind Model 的纯函数式 SQL DSL

> 目标：不是给分析师做 SQL 方言，而是把 Cagara 做成一个 **total、lawful、
> deep-embedded 的 Haskell 风格 DSL**：查询是纯值，组合靠类型类与定律，
> 非法状态在类型里就不可表示，最大化 "类型正确消除运行时错误"。
>
> 这是对上一轮评审的**方向修正**：上一轮按"分析师日常"给了降复杂度建议
> （删符号、改 `select`/`update` 成 SQL 风格）。在这个目标下，那些建议的
> 优先级要反转：不是"更像 SQL"，而是"更像一个有定律的 Haskell 库"。

---

## 0. 三个支柱

| 支柱 | 具体内容 | 解决什么 |
|---|---|---|
| **表示** | 初始代数 / free monad（`Free QueryF`）+ `cata` 解释器 | 查询是可组合的语法值；SQL 只是解释器；归一化 = 自然变换 |
| **代数** | bag ringad（monad + semiring）、`Group`/`foldMap`、`Ordered`/comonad、`Maybe` | 组合律（monad/半环/fold/comonad）+ 聚合/顺序/空值的正确语义 |
| **类型** | `DataKinds` 风格的 row/symbol/type family + GADT phase + 类型类 + 精化/`NonEmpty`/`NonZero`/`Decimal`/`Nat` + `newtype`/ADT | 非法状态不可表示；partial function 变成 `Maybe`/`NonEmpty`；schema/方言/基数/阶段都进类型 |

一句话：

> **Cagara 应该是 "一个带类型级 schema 与 grade 的 bag 代数上的 total
> free monad"，SQL 是它的一个 fold/解释器。**

这和"把 SQL 包成 AST DSL"的区别：每一步组合都是**有定律的态射**，
每一类运行时错误都对应一个**类型构造**，而不是一条运行期检查。

---

## 1. 为什么 Haskell mind model 不是"依赖类型"

Haskell 程序员熟悉的不是 Agda 的 Pi 类型，而是：

- `Monad`/`Applicative`/`Functor`/`Foldable`/`Semigroup`/`Monoid`/`Comonad`；
- `DataKinds`、`GADTs`、`TypeFamilies`、`KnownSymbol`、`HasField`、`row-types`；
- `newtype` + `deriving`、ADT + 模式匹配、类型类 + 定律；
- `Free`/`Fix`/`cata`、`Data types à la carte`、tagless final；
- `Arrow`/`Category` 作为"静态形状"的组合抽象（Hughes）；
- 用 QuickCheck/proptest 检验定律；用编译失败测试检验"非法状态不可表示"。

所以要"更 Haskell"，应该走 **GADT + 类型族 + 类型类 + 定律 + free monad**
这条线，而不是全依赖类型/CQL/范畴数据库。全依赖类型会让 surface 难用、
Rust 实现成本爆炸，而且并不能自动带来 SQL 语义正确性。

另一个重要事实：**Haskell 本身并不消除运行时错误**——`head []`、`fromJust
Nothing`、`undefined`、`Int` 溢出、`error`、异常、`unsafePerformIO` 都是
运行期炸弹。既然目标是"类型正确消除运行时错误"，Cagara 应该做一个
**total 的 Haskell**：

- 不用 `undefined`/`error`/异常；
- 偏函数返回 `Maybe`/`Either` 或收窄为 `NonEmpty`/`NonZero`；
- 算术不静默溢出/截断；`Decimal` 不静默退化成浮点；
- 归一化有终止度量/fuel；
- 不做隐式 coercion；`Maybe`/`Either` 显式。

这比 Haskell 更严格，但保留了它的抽象与组合风格。

---

## 2. 表示：free monad / 初始代数

### 2.1 查询是语法，不是执行

```haskell
data QueryF r a
  = Table   (Table r)
  | Empty
  | Union   (Query r) (Query r)
  | Filter  (Expr Bool) (Query r)
  | Map     (Expr r -> Expr s) (Query r)
  | Join    (Query r) (Query s) (Expr r -> Expr s -> Expr Bool)
  | GroupBy (Expr r -> Expr k) (Query r)
  | FoldMap (Expr a -> Expr m) ...
  | Order   ...
  | Take    ...
  | Pure    a

data Query a = Free QueryF a      -- 或 Fix QueryF
```

`Query` 的 `Functor`/`Applicative`/`Monad` 实例就是组合 API：

- `pure` = 单例行；
- `>>=` = 拼接查询（在 SQL 里通常是 join 或子查询）；
- `empty`/`<|>` = `UNION ALL`/`MonadPlus`；
- `filter`/`guard` = `WHERE`；
- `fmap` = projection；
- `groupBy`/`foldMap` = 分组聚合（单独一层，见 §3.2）。

`>>=` 本身只是**构造语法**；真正的 SQL 由归一化决定。这样既能写
`do`-notation，又能保证"一条 SQL"（LID 的 one-SQL 定理）。

### 2.2 归一化 = 自然变换，后端 = `cata`

```haskell
normalize :: Query a -> Query a
toSQL     :: Query a -> SQL d     -- cata over QueryF
evalBag   :: Query a -> Bag a     -- 参考语义（测试/证明用）
toDoc     :: Query a -> Doc
```

- `normalize` 只用**定律**重写（见 §4），保持类型与语义；
- `toSQL` 是一个代数（algebra），`cata QueryF` 就是解释；
- 任何新后端（DuckDB/Postgres/JSON/lineage）都是另一个 algebra；
- `evalBag` 是参考语义，用来做 property-based 差分；
- `--optimize` 不再依赖 sqlglot 的黑盒，而是"代数重写 + 代价选择形状"。

### 2.3 为什么是 free monad，而不是任意 `Monad`/`IO`

- 任意 `Monad` 的 `>>=` 可能根据**行数据**选择不同的查询/表，这在 SQL
  里不一定能编译成一条语句（query shredding/多语句）；
- free monad 把 `>>=` 记录成语法，归一化可以：
  - 能融合成 SQL 的融合（join/filter/pushdown）；
  - 分支在同一 row 类型时生成 `CASE`；
  - 不能静态确定的，报编译期错误（而不是生成 N 条 SQL 或运行期失败）；
- 这等价于 LINQ 的 expression tree / Ferry 的 arrow / LID 的 quotation，
  但用 Haskell 的 `Free` 表达最自然。

**Arrow/Applicative 的取舍：** 如果希望从类型上就禁止"数据依赖的查询结构"，
可以把用户 surface 限制在 `Applicative`/`Arrow`（`arr`、`>>>`、`first`、
`fanout`），内部仍用 free monad + 归一化。Haskell mind model 里这两条都成立：
`Arrow` 换静态性，`Monad` 换表达力；SQL 需要静态，所以 **surface 偏
Applicative/Arrow，内部 free monad**。

---

## 3. 代数：把 SQL 的每一层变成 Haskell 结构

### 3.1 关系层：bag ringad

```haskell
newtype Rel r = Rel (Bag r)
-- Functor, Applicative, Monad, MonadPlus, Alternative
-- Semigroup/Monoid  : unionAll (<>) / empty
-- Semiring          : 0 = empty, 1 = singleton, + = unionAll, * = cartesian
```

- `Rel` 是 **bag**（不是 list/set）；`Monad` 给出 comprehension/`do`；
- `Semigroup`/`Monoid` 给出 `UNION ALL`；
- 半环给出 `JOIN`、分配律、重排律；
- `do { x <- q1; y <- q2; guard p; pure e }` = SQL 的
  `FROM q1, q2 WHERE p SELECT e`；这是 Haskell 程序员最熟的查询写法。

### 3.2 分组层：`Map` + `NonEmpty` + `foldMap`

```haskell
type Group k a = Map k (NonEmpty a)          -- 组非空，这是 SQL 的事实
groupBy :: Ord k => (a -> k) -> Rel a -> Group k a
  -- 实现：foldMap (\a -> singleton (k a) (a :| []))

foldMap1 :: Semigroup m => (a -> m) -> NonEmpty a -> m
foldMap0 :: Monoid m    => (a -> m) -> Rel a -> Maybe m
```

- `sum`/`avg`/`min`/`max` 是 **`Semigroup` 折叠**：组非空 → 结果非空，
  不需要 `coalesce 0.0 (sum .amount)`；
- `count` 是 `Monoid` 折叠到 `Int`（0 是单位）；
- 窗口帧可能为空 → `foldMap0` 返回 `Maybe`；
- `avg` 用 `(sum, count)` 半群，非空组天然非空；
- `GROUP BY` + `HAVING` = `groupBy` + `foldMap` + `filter`，没有特殊 stage。

这一层是 **`Foldable`/`Semigroup`/`Monoid`** 的组合，不是 monad bind；
这也解释了为什么"SQL 是 monad"这个说法在 `GROUP BY` 处失效。

### 3.3 呈现层：`Ordered` + comonad

```haskell
newtype Ordered a = Ordered [a]              -- 带排序见证；允许空结果
sortOn :: Ord k => (a -> k) -> Rel a -> Ordered a
take   :: Int -> Ordered a -> Ordered a
drop   :: Int -> Ordered a -> Ordered a
```

- `ORDER BY`/`LIMIT`/`OFFSET` 是 `Ordered` 上的 list 操作；
- `limit`/`offset` 合成一个 `page skip take`（避免"先取 20 再跳 20 = 空"）；
- **没有 `Ordered` 就不能 `take`**：未排序分页是类型错误（SQL 里顺序本来不稳定）；
- 窗口函数是 `Ordered`/分区结构上的 **comonad**：
  `duplicate :: Win a -> Win (Win a)`（每个位置 → 它的帧），
  `extract :: Win a -> a`（当前行）；`lag/lead/running sum/rank` 都是
  `extend`/`duplicate` 的态射；窗口不能嵌套 = `duplicate` 的层级事实。

### 3.4 可空层：`Maybe` + 非空谓词

- `Maybe` 对应 SQL NULL；`Functor`/`Applicative`/`Monad` 照常；
- **谓词要求 `Bool`（非空）**，`Maybe Bool` 只能通过 `whereTrue`/`isTrue`
  显式降级；3VL 被限制在边界，不在整个语言里蔓延；
- 聚合的目标 monoid 取 `Maybe m` 时自动跳过 `Nothing`（SQL 聚合跳过 NULL）；
  全 `Nothing` 得 `Nothing`，与 `sumMaybe` 一致；
- `GROUP BY` 的 nullable key 用 `Maybe k` 的 `Ord`（NULL 是一组），
  但 **不要继承 Haskell `Maybe` 的 `Ord`（Nothing 最小）**，要用显式
  `NullsFirst`/`NullsLast` 类型；SQL 的 NULL 排序是可配置的。

---

## 4. 类型系统：DataKinds/GADTs/type families 风格

### 4.1 类型级 row / schema

Cagara 已经有行类型与 `keyMap`/`removeKey`/`merge`/`mapValue`；Haskell 化是
把它们从"检查器里的 reducer"提升为**类型级函数**：

```haskell
type family Merge (r :: Row) (s :: Row) :: Row
  -- 冲突时 TypeError "duplicate label"
type family Remove (l :: Symbol) (r :: Row) :: Row
type family KeyMap (m :: Mapper) (r :: Row) :: Row
type family MapValue (w :: Wrapper) (r :: Row) :: Row
type family Disjoint (r :: Row) (s :: Row) :: Constraint
```

收益：

- join 同名列不再是"左胜"的静默行为，而是 `Disjoint`/`Merge` 的**类型错误**；
- `select`/`update`/`omit`/`prefix` 的 row 等式在类型里；错误信息是
  "row 冲突"，不是运行期少一列；
- `Table` 的 schema 是类型级 row，由 schema 生成器产生；`table` 值携带
  `Table schema`；列名是 `KnownSymbol`，不是运行期字符串；
- `.field` 访问是 `HasField`/`row-types` 风格；join 两侧用限定 label
  （`L "u" r` / `L "m" s`）或 `Qualified` row，输出显式 `Merge`；
- schema 漂移：`Table` 类型由 DB/DDL 生成，`check --schema` 在编译期对比；
  手写"断言式 schema"无法消除漂移，类型级生成才可以。

### 4.2 相（phase）→ GADT / grade

```haskell
data Phase = Row | Agg | Win | Invalid      -- Invalid 由无方程的 Combine 产生
data Expr (p :: Phase) r a where
  Column :: HasField l r a => Expr 'Row r a
  Sum    :: Expr 'Row r Int -> Expr 'Agg r (Maybe Int)
  ...
type family Combine (p :: Phase) (q :: Phase) :: Phase
  -- Row ⊗ Row = Row; Row ⊗ Agg = Agg;
  -- Agg ⊗ Agg / Win ⊗ Win / Agg ⊗ Win 没有方程（stuck → 类型错误）
```

- 一张 `Combine` 表替代 `rules.rs` 的手写组合规则；
- 聚合/窗口不能嵌套是**类型级不可表示**，不是运行期检查；
- 新加 stage 只需声明 grade；错误信息直接说 "Agg 里不能再 Agg"。

### 4.3 方言 → 类型类 capability

```haskell
data Dialect = Postgres | DuckDB | SQLite | BigQuery | ...
data Feature = FullJoin | NullsFirst | Decimal | TryCast | ...
class Supports (d :: Dialect) (f :: Feature)
  -- FullJoin, NullsFirst, Decimal, TryCast, ...
fullJoin :: Supports d 'FullJoin => ... -> SQL d
```

- `--dialect` 变成类型级/字典；未验证方言 + 未支持特性 = **类型错误**；
- 不再出现"oracle 生成 DATE_TRUNC、hive 生成 OFFSET"这种静默承诺；
- `ansi` 改成 `Generic`/`Postgres`，`Verified`/`Generated` 用类型区分；
- 测试矩阵 = capability 的实例矩阵，编译器强制覆盖。

### 4.4 基数 → grade（可选）

```haskell
data Card = One | Many
Rel (c :: Card) r
joinOne  :: Rel 'One  r -> ...
joinMany :: Rel 'Many r -> ...
-- 1·ω = ω; ω·ω = ω; 聚合把 Many 折成 One
```

配合 schema 的 `primaryKey`/`joinKey` 类型级声明，静态发现"一对多 join 后
按左表 key 聚合导致重复计数"。这是 SQL 自身没有、Malloy 用约定保证的东西；
类型级化之后是编译错误。

### 4.5 精化 / totality

```haskell
NonEmpty a, NonZero a, Nat, Decimal p s, Between lo hi
safeDiv :: Int -> Int -> Maybe Int
div     :: Int -> NonZero Int -> Int
```

- `NonEmpty`：聚合非空、`head`/`last`/`firstValue` 不再返回裸值；
- `NonZero`：除法/取模总函数；schema 有 `CHECK (x <> 0)` 时才能收窄；
- `Nat`：`limit`/`offset`/`page` 不能为负（类型错误，不是运行期检查）；
- `Decimal p s`：金额精度；`float` 与 `Decimal` 不能静默混算；
- `newtype UserId`/`OrderId`/`Money`：不同 ID/单位不能混用；
- `data Status = Paid | Open | Void`：状态是 ADT，不是裸 string；
  `fromString :: String -> Maybe Status`；`case` 穷尽；
- `Maybe`/`Either` 显式；没有 `undefined`/`error`/异常；
- 算术：`Int64` 上溢/除零要么 `Maybe`，要么用 `Integer`/`Decimal` 并在
  SQL 后端选择安全 lowering；引擎差异用 capability + 测试兜底。

### 4.6 类型类与定律

暴露一个**封闭的 prelude 类层次**（用户可以 `deriving`，但先不允许自定义实例）：

```haskell
Functor, Applicative, Monad, MonadPlus, Alternative
Semigroup, Monoid, Foldable
Semiring (或 Ringad: 0/1/+/×)
Comonad, Category, Arrow
SqlRep (新类型 → SQL 表示), KnownSymbol, HasField
```

每条定律都用 property test 检验：

- `Functor`: `fmap id = id`，`fmap (f . g) = fmap f . fmap g`；
- `Monad`: 左/右单位、结合律；
- `Semigroup`/`Monoid`: 结合、单位、交换（`unionAll` 交换）；
- `Semiring`: 分配、0/1；
- `Comonad`: `extract . duplicate = id` 等；
- `Category`: `id >>> f = f`，结合律；
- `Arrow`: `arr id`、组合、`first` 等。

Haskell mind model 的核心不是"有 type class"，而是**类 + 定律 + 可替换实例**
（`evalBag`、`toSQL`、`toDoc` 是同一个代数的不同实例）。

---

## 5. 类型正确消除运行时错误：一张对照表

| 运行期错误 | 类型级消除方式 |
|---|---|
| 少写/拼错列、未知类型名 | 类型级 row + `KnownSymbol`；未知类型名报错（不是自由变量） |
| join 同名列静默丢右侧 | `Disjoint`/`Merge` 类型错误；限定 label 显式 `Merge` |
| 未分组列 | `Group`/`agg` 类型（已有，强化） |
| 聚合/窗口嵌套 | phase GADT/`Combine` 类型族 |
| 谓词里出现 NULL | 谓词类型是 `Bool`；`Maybe Bool` 只能显式 `whereTrue` |
| `sum`/`avg`/`min`/`max` 为 NULL | `NonEmpty`/`Fold1` → 非空组非空；`Fold0` 才 `Maybe` |
| 空窗口帧 | `Fold0` → `Maybe`（明确） |
| 除零 | `NonZero` 精化 + `safeDiv`/`Maybe`；无裸 `/` 偏函数 |
| 整数溢出 | `Integer`/`Decimal` 或 checked `Maybe`；capability/测试 |
| `toDecimal` 仍是 float、和 double 混算 | `Decimal p s` phantom type；禁止隐式混算 |
| 负数 `limit`/`offset` | `Nat`/`NonNegative` 精化 |
| 未排序就分页 | 只有 `Ordered` 才有 `take`/`page` |
| schema 漂移 | 类型级 `Table` 由 DB/DDL 生成；`check --schema` 编译期对比 |
| 未验证方言 | `Supports d Feature` 约束；不满足即类型错误 |
| 非法状态字符串 | `data Status = ...` ADT；`fromString` 返回 `Maybe` |
| SQL 注入 | `Param`/`Code` quotation；无字符串拼接 |
| 一对多 join 后重复计数 | `Card` grade + `primaryKey`；静态错误/警告 |
| 混用不同 ID/单位 | `newtype UserId`/`Money` + `SqlRep` |
| `lpad` 参数序错 | 同类参数无法靠类型区分；用 record 参数或 newtype（`Width`/`Fill`） |
| `dayOfWeek` 方言差 | `DayOfWeek` ADT + 方言 capability；或语言内用 `truncWeek`/`daysBetween` 计算 |
| 归一化不终止 | fuel/终止度量；递归只允许受控 `Fix`/stratified；否则报告编译器限制 |

**诚实边界：**

- DB 连接/权限/网络/资源耗尽不是纯查询错误，留在核心之外；
- 数据实际取值违反 schema（如除零、非法 enum）只有 schema 约束/`NonZero`
  才能静态消除；否则显式 `Maybe`；
- 引擎语义（溢出、decimal、NULL 排序、方言）只能靠 capability + 一致
  lowering + 引擎测试；类型系统不能证明所有引擎行为；
- 全 SQL 等价性不可判定；用定律 + 差分 + Cosette 证明**片段**。

---

## 6. Surface：更 Haskell，而不是更像 SQL

### 6.1 `do` 与 combinators

```haskell
-- 关系层：do = comprehension；guard = WHERE；pure = SELECT
adults = do
  u <- users
  guard (u.age >= 18 && u.active)
  pure (id = u.id, name = upper u.name)

-- unionAll = <|>
everyone = adults <|> archived

-- join = bind + guard（归一化识别成 equijoin）
order_names = do
  o <- orders
  u <- users
  guard (o.user_id == u.id)
  pure (order_id = o.id, name = u.name)

-- 聚合 = groupBy + foldMap（不是特殊 stage）
revenue = do
  o <- orders
  guard (o.status == "paid")
  pure (user_id = o.user_id, amount = o.amount)
  & groupBy .user_id
  & fmap (second (foldMap1 sum))
  & toRel

-- 呈现层：sortOn/take
top = revenue & sortOn (desc .total) & take 10
```

- `&` / `$` / `>>>` / `<<<` / `let` / `case` 都保留（本来就是 Haskell）；
- `<>` 是 **Semigroup** 拼接（string、bag、list 通用），不是 SQL 的 `<>`；
  不等用 `/=` 或 `!=`；
- `select`/`update` 让位给 `fmap`/record update（或保留为 sugar）；
- `agg` 让位给 `groupBy` + `foldMap`；
- `order`/`limit` 让位给 `sortOn`/`take`（`Ordered`）；
- `where` 让位给 `filter`/`guard`；符号简写 `&? &= ...` 如果保留，只作为
  这些**有定律算子**的别名，而不是第二套 SQL stage 名。

### 6.2 用户类型

Haskell mind model 需要领域类型；Cagara 现在"没有用户类型"会漏掉一大块
类型安全。建议按性价比加：

1. `type` 别名（row 别名）；
2. `newtype`（`UserId`/`Money`/`Days`）+ `deriving SqlRep`；
3. `data` 枚举（`Status = Paid | Open | Void`）+ 穷尽 `case`；
4. 暂不做一般递归 ADT/record（SQL 编码复杂），除非有明确需求。

### 6.3 类型类

先封闭 prelude 类（`Functor`/`Monad`/`Semigroup`/`Foldable`/`Comonad`/
`Category`/`Arrow`/`SqlRep`），允许 `deriving`，不允许用户自定义实例；
等语言成熟再开放。这保留 Haskell 的抽象，又不引入 overlapping/orphan 的
复杂度。

---

## 7. 映射回 Cagara / 路线图

### 7.1 已经有的（保留）

| 现有 | Haskell 化后的位置 |
|---|---|
| HM + row types + open row | `Rel (r :: Row)` + `HasField`/`KnownSymbol` |
| `expr`/`agg`/`win` phase | `Expr (p :: Phase)` + `Combine` 类型族 |
| normalizer + arena | `Free QueryF` + `cata` 归一化 |
| `sql` 模板 + 签名 | typed holes；escape hatch（保留） |
| bag 语义 | `Rel` 的 ringad 语义 |
| `&`/`$`/`>>>`/`let`/`case` | 保留，Haskell 风格 |
| `doc`/`fmt`/LSP | 保留；增加编译失败测试与定律测试 |

### 7.2 要改的（按"类型正确"优先）

1. **row 类型级化**：`Merge`/`Disjoint`/`Remove`/`KeyMap` 变成类型函数；
   join 冲突是类型错误；schema 由 DB/DDL 生成成 `Table schema`。
2. **`Group`/`NonEmpty` + `Fold1`/`Fold0`**：聚合非空；消掉 coalesce 税。
3. **phase GADT/grade**：`Combine` 表；替换 `rules.rs` 的手写组合。
4. **`Ordered` + `Nat` + `page`**：顺序进类型；分页正确。
5. **totality**：`safeDiv`/`NonZero`/`Decimal p s`/`Maybe`/`Either`；
   禁止 `undefined`/异常/静默溢出；归一化 fuel。
6. **两级参数**：`Param a`/`Code (Expr r a)` + quotation；修参数组合 bug。
7. **方言 capability**：`Supports d Feature`；未验证方言/特性类型错误。
8. **`newtype`/ADT/封闭类**：领域类型与定律层。
9. **`do`/comprehension + combinator surface**：`fmap`/`filter`/`<|>`/
   `foldMap`/`groupBy`/`sortOn`/`take`；符号简写降级为 sugar。
10. **free monad/`cata` 后端**：`evalBag`/`toSQL`/`toDoc` 三个实例；
    `Rel -> Rel` 归一化 pass；`--optimize` 变成代数重写/e-graph。

### 7.3 最小实验顺序

1. `Group`/`Fold1` + `NonEmpty`（S–M）：先证明 coalesce 税可消；
2. row `Disjoint`/`Merge` 类型级冲突（S–M）：先证明 join 静默丢列可消；
3. phase `Combine` 类型族（M）：先证明嵌套规则可类型化；
4. `Ordered`/`page` + `Nat`（S–M）：先证明分页 bug 可消；
5. `safeDiv`/`Decimal`/`Maybe` totality（M）：先证明 partial function 可消；
6. `Param`/`Code` staging（M–L）：修两个参数 bug；
7. `Free`/`cata` + `do` surface（L）：最后的形态统一。

每一步都配**编译失败测试**（非法状态）和**定律 property test**（合法状态），
这是 Haskell mind model 的"类型正确"落地方式。

---

## 8. 不要做什么

- **全依赖类型/Agda 风格**：Haskell 的 `DataKinds`+GADT+类型族已足够；
- **CQL/范畴数据库当 surface**：适合语义/血缘，不适合日常查询；
- **把 `IO`/effects 放进查询核心**：查询是纯值；执行在解释器外；
- **用 set 语义**：SQL 是 bag；set 丢 `UNION ALL`/聚合重复计数；
- **把 `Maybe` 的 Haskell `Ord` 当 SQL NULL 排序**：SQL 是显式
  `NULLS FIRST/LAST`；
- **允许任意 `Monad` 的 data-dependent 查询结构**：SQL 生成不了单条语句；
  用 free monad + 归一化/arrow/applicative 限制；
- **保留 `undefined`/`error`/静默溢出/隐式 coercion**：这不是"Haskell 风格"，
  是 Haskell 的缺陷；total 才是目标；
- **为了理论而重写内核**：Cagara 的 arena/normalizer 已经是初始代数；
  逐层类型化即可。

---

## 9. 参考

1. **Hughes, "Generalising Monads to Arrows"**（2000）——static shape 的组合抽象。
2. **Meijer, "The Essence of LINQ"**——monad comprehension 作为查询语言。
3. **Cheney, Lindley, Wadler, "A Practical Theory of Language-Integrated Query"**
   （ICFP 2013）——quotation + normalization + one-SQL 定理。
   [PDF](https://homepages.inf.ed.ac.uk/slindley/papers/practical-theory-of-linq.pdf)
4. **Kiselyov 等, "Finally tagless, partially evaluated"**——type class 多解释器。
5. **Swierstra, "Data types à la carte"**（2008）——可扩展 AST / free monad。
6. **Gibbons, "Comprehending Ringads"**（LNCS 9600, 2016）——bag 的 monad + semiring 定律。
7. **Grust, "Monad Comprehensions: A Versatile Representation for Queries"**（2004）——
   comprehension → SQL。
   [PDF](https://db.in.tum.de/~grust/files/monad-comprehensions.pdf)
8. **Uustalu, Vene, "The Essence of Dataflow Programming"**（2005）——comonad/窗口。
9. **Hutton, "A tutorial on the universality and expressiveness of fold"**（1999）——
   `foldMap`/聚合。
10. **Bird, de Moor, "Algebra of Programming"**（1997）——等式推理/程序推导。
11. **Turner, "Total Functional Programming"**（2004）——无 bottom/异常的 total 语言。
12. **Liquid Haskell**——轻量精化类型（`NonEmpty`/`NonZero`/`Nat` 的参考）。
13. **HaskellDB / Links / Ferry**——typed SQL embedding 的工程先例。
    [Ferry](https://db.cs.uni-tuebingen.de/research/past-projects/ferry/) ·
    [Cosette](https://www.cidrdb.org/cidr2017/papers/p51-chu-cidr17.pdf)（SQL 等价性）
14. **Malloy**（[malloydata.dev](https://www.malloydata.dev/)）——schema/基数/嵌套的工程参照
    （但用类型而不是约定来保证）。

---

## 10. 一句话总结

> 目标不是"更像 SQL"，而是 **"一个 total 的 Haskell：查询是 free monad 的
> 语法值，组合由 lawful type classes 驱动，schema/phase/方言/基数/精度
> 全部进类型，SQL 后端只是 `cata` 出来的一个解释器。"**
>
> 这样，可组合性来自 monad/半环/fold/comonad 的定律；
> 类型正确性来自 row/type family/GADT/精化/`newtype`/ADT；
> 而运行时错误只剩下环境、真实数据违约、引擎语义这三类**本该显式暴露**的东西。