# 类型系统研究：ReaderT rowShape、无糖表达式与可检查性

> 目标：研究 Cagara 的类型系统应该怎么设计。约束是：
>
> 1. **`.field_name` 由 Reader/ReaderT rowShape 的隐式自动 lift 实现**
>    （即表达式本质上是在行环境上的 reader）；
> 2. **尽量不要糖**：不要 `summarize`/`window`/`select`/`update` 这类第二套写法，
>    也不要为每个 SQL 算子造一个特制语法；用类型类、函数、`do`/`fmap` 表达；
> 3. 保持 pure FP 的可组合性 + 类型正确性（尽量消除运行期错误）。
>
> 本文只研究**类型系统本身**：`Expr`/`ReaderT` 的类型、自动 lift、行类型、
> phase grade、参数舞台、NULL/totality、deep vs tagless、以及"无糖"的边界。
> 不做形式化证明；只给出类型规则、定律陈述、设计取舍与开放问题。

---

## 0. 结论先行

1. **`expr r a ≅ ReaderT r ExprM a` 是对的，但只是"语义"视角。**
   `.field` = `asks`/`getField`；所有**同结果类型**的算子（`Num`、`Semigroup`、
   `Monoid`、`IsString`）可以靠 `Applicative`/类型类自动 lift；
   **比较/布尔算子**（`Eq`、`Ord`、`&&`、`||`、`not`）不能直接用 Haskell 的
   类，因为它们的返回类型要变成 `Expr r Bool`，需要**lifted 类**
   （`LiftEq`/`LiftOrd`/`LiftBool`）或显式 `liftA2`。
2. **ReaderT 的语义不等于可检查的语法。**
   如果 `Expr r a` 真的只是 `r -> a`，SQL 生成器看不到 `.field` 是哪一列。
   要生成 SQL，`ExprM` 必须是**可检查的表达式代数**（free/initial algebra 或
   tagless final），`.field` 必须是 `Field` primitive；`ReaderT` 负责语义，
   不负责语法。
3. **查询层最好不是普通 `Monad`，而是按输入/输出行索引的 `Query r s`**
   （indexed monad / 关系范畴）：`pure = 恒等关系`，`>>=` 是关系复合，
   `filter`/`map`/`join` 有清晰的类型；普通 `Rel r` monad 在 `do`/`pure`
   处会遇到"`pure` 接受的是行值还是行表达式"的问题。
4. **无糖的核心**：`do`/`>>=`/`fmap`/`pure`、`field`/`HasField`、
   `liftA2`/lifted 类、`groupBy`/`foldMap`/`sortOn`/`take`；
   phase 用 **类型级 grade**；参数用 **两个 Reader（Param/Row）**；
   `summarize`/`window`/`select/update`/符号简写都不进核心。
5. **三个必须解决的类型论问题**：
   (a) Reader 的可检查性（deep vs tagless）；
   (b) lifted `Eq/Ord/Bool` 与类型类一致性/默认化；
   (c) 行多态 + `HasField` 的推断与歧义。

---

## 1. `.field` 的 ReaderT 语义

### 1.1 类型

```haskell
type Expr (r :: Row) a = ReaderT r ExprM a
-- 或者：newtype Expr r a = Expr (ReaderT r ExprM a)

ask   :: Expr r r
asks  :: (r -> a) -> Expr r a
local :: (r' -> r) -> Expr r a -> Expr r' a
field :: HasField l r a => Label l -> Expr r a
field = asks (getField @l)
```

- `r` 是**行环境**（rowShape）；`a` 是表达式结果；
- `.field` 就是 `field @"name"`，是 `asks`/`getField` 的别名；
- `Expr` 是 `ReaderT`，所以自动有 `Functor`/`Applicative`/`Monad`。
- `local`/`reindex` 改变行环境（用于 join/别名/子查询/投影后的重新索引）。

### 1.2 自动 lift：哪些能，哪些不能

**能自动 lift（结果类型不变）：**

```haskell
instance Num a       => Num (Expr r a)        -- (+) = liftA2 (+), fromInteger = pure . fromInteger
instance Fractional a => Fractional (Expr r a)
instance Semigroup a => Semigroup (Expr r a)  -- (<>) = liftA2 (<>)
instance Monoid a    => Monoid (Expr r a)     -- mempty = pure mempty
instance IsString (Expr r String)             -- fromString = pure . fromString
```

这些是 `Applicative` 的直接结果：`liftA2`/`pure`；不需要特制语法。

**不能直接用 Haskell 类（结果类型要变）：**

```haskell
-- Haskell 的 Eq 是 a -> a -> Bool，不能变成 a -> a -> Expr r Bool
-- 所以需要 lifted class：
class LiftEq a where
  (.==) :: Expr r a -> Expr r a -> Expr r Bool
class LiftOrd a where
  (.<), (.<=), (.>), (.>=) :: Expr r a -> Expr r a -> Expr r Bool
class LiftBool where
  (.&&), (.||) :: Expr r Bool -> Expr r Bool -> Expr r Bool
  lnot         :: Expr r Bool -> Expr r Bool
```

或者不用类，直接写 `liftA2 (==) :: Expr r a -> Expr r a -> Expr r Bool`。
**这是类型系统的关键点：`Eq`/`Ord`/布尔运算不能靠 `Applicative` 自动 lift
到"表达式结果"，必须显式提供 lifted 类/函数。**

`ifThenElse`/`case` 类似：它们的返回类型是 `Expr r a`，但条件/分支是表达式；
需要 primitive/类：

```haskell
ifThenElse :: Expr r Bool -> Expr r a -> Expr r a -> Expr r a
```

**`Maybe` 在值层，不在 `ReaderT` 层：**

```haskell
just, nothing :: a -> Expr r (Maybe a) / Expr r (Maybe a)
coalesce      :: Expr r a -> Expr r (Maybe a) -> Expr r a
isNull        :: Expr r (Maybe a) -> Expr r Bool
```

不要把 `MaybeT` 叠在 `ReaderT` 上（那会让整个表达式可能失败）；SQL NULL 是
**每个值**的 `Maybe`，不是表达式求值失败。

### 1.3 ReaderT 的类型论

- `ReaderT r m` 是 **monad transformer**；`MonadReader r (ReaderT r m)`；
- `Reader r a = ReaderT r Identity a`；语义是 `r -> a`；
- `ReaderT` 在 `r` 上是**逆变**的（`local`/`contramap`），在 `a` 上是协变的；
  所以 `Expr` 是一个 **profunctor** `(r ↝ a)`；`local` = `lmap`，`fmap` = `rmap`；
  `Strong`/`Profunctor` 可以用于组合；
- `Reader` 是 **monad**；对偶的 **Env/Coreader**（`Env r a = (r, a)`）是
  **comonad**：`ask`/`extract`、`local`/`extend`。表达式层用 Reader（需要
  `ask` 和 `local`）；窗口/邻域层用 Env/Store comonad（见 §6）；
- `ReaderT` 的定律：

```text
ask >>= k        = k ask                    -- 或 reader 定律
local id         = id
local f . local g = local (g . f)
asks f           = fmap f ask
local f (pure x) = pure x
```

- `ReaderT` 是纯的；没有 effects；`ExprM` 只是一个"表达式效果的 monad"
  （Identity/AST/tagless），不是 IO。

---

## 2. 可检查性：ReaderT 不能直接生成 SQL

如果 `Expr r a = r -> a`（shallow），`.field` 是 `asks`，自动 lift 很漂亮，
但 **SQL 生成器看不到表达式结构**：它只知道一个 Haskell 函数，不知道
"这是 `name` 列、那是 `+`、那是 `SUM`"。所以必须区分三个层次：

| 架构 | `Expr r a` 表示 | `.field` | 自动 lift | 可检查/SQL | 归一化 |
|---|---|---|---|---|---|
| **Shallow Reader** | `r -> a`（或 `ReaderT r Identity`） | `asks`/`getField` | `Num`/`App` 等 | ✗（不透明） | 需要 quotation/LID |
| **Deep Reader** | `Free ExprF` / `Fix ExprF`（reader-indexed） | `Field` primitive | `App`/`Prim` 类 | ✓ | ✓ |
| **Tagless final** | `forall m. ExprLang m => m r a` | 类方法 `field` | 类方法 | 实例可检查；无统一 AST | 依赖实例 |
| **Hybrid (LID)** | 宿主 `Reader` + `Code (Expr)` | `asks`/quotation | 宿主/类 | 通过 quotation | ✓ |

### 2.1 Deep Reader（推荐给编译器）

```haskell
data ExprF (r :: Row) a where
  Field :: HasField l r a => Label l -> ExprF r a
  Lit   :: a -> ExprF r a
  Prim  :: Prim a -> ExprF r a            -- +, ==, and, ...
  App   :: ExprF r (a -> b) -> ExprF r a -> ExprF r b
  If    :: ExprF r Bool -> ExprF r a -> ExprF r a -> ExprF r a
  Local :: RowMap r' r -> ExprF r a -> ExprF r' a   -- 类型级 reindex

newtype Expr (r :: Row) a = Expr (Free (ExprF r) a)
```

- `Field` 携带类型级 label；`HasField` 约束保证存在；
- `Prim`/`App` 提供 `Num`/`Semigroup` 等的语法；
- `Local` 用**类型级 row map**（不是宿主函数），这样 SQL 生成器能解释
  scope/alias；
- 语义：`eval :: Expr r a -> r -> a`（或 `r -> m a`），`toSQL :: Expr r a -> SQL`；
- `ReaderT` 是语义解释，不是语法表示；`.field` 在语义上就是 `asks`，
  在语法上就是 `Field`。

### 2.2 Tagless final（推荐给库/预置库）

```haskell
class ExprLang (m :: Row -> Type -> Type) where
  field :: HasField l r a => Label l -> m r a
  lit   :: a -> m r a
  prim  :: Prim a -> m r a
  app   :: m r (a -> b) -> m r a -> m r b
  if_   :: m r Bool -> m r a -> m r a -> m r a
  local :: RowMap r' r -> m r a -> m r' a

newtype Expr r a = Expr (forall m. ExprLang m => m r a)
```

- 多个解释器：`SQL`、`Eval`、`Pretty`、`JSON`；
- 没有统一 AST，归一化困难；适合预置库/宿主嵌入，不适合需要重写的编译器核心；
- `.field`/lift 都是类方法；无糖。

### 2.3 Shallow Reader / LID

- `Expr r a = ReaderT r Identity a`；`.field` = `asks`；`Num`/lift 靠函数实例；
- 不能检查；SQL 生成用 **quotation/normalization**（LID）：把查询写成
  quoted term，归一化成一条 SQL；
- 适合宿主语言/参考解释器，不适合独立的 SQL 编译器核心；
- 对参数/反引用最自然（`~p`），能修"参数经 projection 丢失"的 bug。

**建议：**

- **核心 = Deep Reader**（可检查、可归一化、可生成 SQL）；
- **预置库/多后端 = Tagless final**（同一组类，多个解释器）；
- **参数/宿主嵌入 = LID quotation**（两个 Reader：`Param` 与 `Row`）；
- `.field` 在所有架构里都保持"Reader 语义 + `HasField` 类型"，不引入特制语法。

---

## 3. 核心类型系统骨架

### 3.1 Kinds

```text
Kind:
  Type
  Row
  Phase          -- Row | Agg | Win
  Card           -- One | Many（可选）
  Dialect
  Semiring       -- Nat | Bool | Provenance（可选）
```

### 3.2 Types

```text
A, B ::= Int | Bool | String | Date | Decimal p s
       | Maybe A | A × B | { l : A, ... }
       | Expr r A
       | Rel r | Group k r | Ordered r | Win r A
       | Param A
       | Code (Expr r A)

r ::= { l : A, ... } | r1 ∪ r2 | r \ l | rho
```

- `Expr r A`：行环境 `r` 上的表达式；
- `Rel r`：bag of row `r`（或者索引版 `Query r s`，见 §3.4）；
- `Group k r`：非空组；
- `Ordered r`：有序呈现；
- `Win r A`：窗口/comonad；
- `Param A`：宿主/编译期参数；
- `Code (Expr r A)`：quoted expression（LID）。

### 3.3 Expression 类型规则（示意）

```text
Γ, l : A ∈ r
-----------------------------  (HasField)
Γ ⊢ field @l : Expr r A

Γ ⊢ e : Expr r A        A 有 Num 实例
----------------------------------------  (Num lift)
Γ ⊢ e + e' : Expr r A

Γ ⊢ p : Expr r Bool      Γ ⊢ a : Expr r A      Γ ⊢ b : Expr r A
---------------------------------------------------------------
Γ ⊢ if p then a else b : Expr r A

Γ ⊢ p : Expr r Bool       Γ ⊢ q : Expr r Bool
---------------------------------------------  (LiftBool)
Γ ⊢ p .&& q : Expr r Bool

Γ ⊢ x : Expr r A     Γ ⊢ y : Expr r A
--------------------------------------  (LiftEq)
Γ ⊢ x .== y : Expr r Bool
```

- `Num`/`Semigroup`/`Monoid`/`IsString` 的 lift 是 `Applicative`/类实例；
- `Eq`/`Ord`/`Bool` 是 lifted 类；
- `field` 是 `HasField`；
- `local` 是 row map；
- 没有特制表达式语法节点（除了 primitive `Prim`/`If`/`Field`）。

### 3.4 Query 层：`Rel r` 还是 `Query r s`？

这是一个需要拍板的类型论问题。

**候选 A：`Rel r` 是普通 monad。**

```text
pure  : r -> Rel r
(>>=) : Rel r -> (r -> Rel s) -> Rel s
filter: Expr r Bool -> Rel r -> Rel r
map   : Expr r s -> Rel r -> Rel s
```

问题：`do x <- q; pure e` 里 `e` 是行表达式 `Expr r s`，但 `pure` 需要
一个**行值** `s`，不是 `Expr r s`；如果 `e` 是宿主函数，SQL 不可检查；
如果 `e` 是 AST，`pure` 不能接受它。

**候选 B：索引 monad / 关系范畴 `Query r s`。**

```text
pure  : Query r r                    -- 恒等关系
(>>=) : Query r s -> (s -> Query s t) -> Query r t
filter: Expr r Bool -> Query r r
map   : Expr r s -> Query r s
join  : Query r s -> Query s t -> Query r t
```

- `Query r s` 是"从输入行 `r` 到输出行 `s` 的关系"；
- `do x <- q; k x` 中 `x : s`（行），`k : s -> Query s t`；
- `pure` 是恒等；`map e` 是投影/单例；`filter`/`join` 是组合；
- `Query () r` 是闭合查询；`Rel r` 可以是 `Query () r`；
- 这是 **indexed monad / parameterized monad**（Atkey）或关系范畴；
- `do` 的类型规则：

```text
Γ ⊢ q : Query r s      Γ, x : s ⊢ k : Query s t
------------------------------------------------
Γ ⊢ do x <- q; k : Query r t

Γ ⊢ e : Expr r s
----------------
Γ ⊢ map e : Query r s
```

- 好处：`do`/`pure`/`>>=` 都有精确类型；`select`/`project` 是 `map`；
  `join` 是 `Query` 复合；SQL 生成有输入/输出行；
- 代价：不是标准 `Monad` 类（`pure`/`>>=` 的索引规则不同），需要
  `IxMonad`/`Category`/自定义 `do` 规则；`Rel r` 只是特例。
- 这比"`Rel` monad + `Expr` reader"更精确；建议作为核心类型，`Rel r` 作为
  语法糖/特例（`Query () r`）。

**候选 C：`Rel r` + `Expr` reader + 显式 `select`。**

- `do x <- q; ...; select e` 其中 `select : Expr r s -> Rel r -> Rel s`；
  `pure` 只用于宿主行值/常量；`select` 用于表达式；
- 简单，但 `do`/`pure` 的"表达式"语义不统一；`select` 不是核心 monad 操作；
- 可能是最容易实现的折中。

> 我倾向 **候选 B**（`Query r s` 索引 monad）：它把"关系复合"和
> "reader 表达式"分开，`do`/`pure`/`map`/`filter` 类型清楚，且能自然表达
> join/子查询；`Rel r = Query () r` 是闭合查询的特例。

---

## 4. 自动 lift：类型类设计

### 4.1 同结果类型的 lift（可以直接用实例）

| 类/操作 | 结果 | 实现 |
|---|---|---|
| `Num`/`Fractional` | `Expr r a` | `liftA2`/`fmap`/`pure` |
| `Semigroup`/`Monoid` | `Expr r a` | `liftA2 (<>)` / `pure mempty` |
| `IsString` | `Expr r String` | `pure . fromString` |
| `Field`/`HasField` | `Expr r a` | `asks`/`Field` primitive |
| `local`/`reindex` | `Expr r' a` | `contramap`/row map |
| `ifThenElse` | `Expr r a` | primitive/class |

### 4.2 结果类型变化的 lift（需要 lifted 类）

| 操作 | 结果 | 设计 |
|---|---|---|
| `==`/`/=` | `Expr r Bool` | `LiftEq` |
| `<`/`<=`/`>`/`>=` | `Expr r Bool` | `LiftOrd` |
| `&&`/`||`/`not` | `Expr r Bool` | `LiftBool` |
| `isNull`/`isNotNull` | `Expr r Bool` | `LiftMaybe` |
| `coalesce`/`fromMaybe` | `Expr r a` | `LiftMaybe` |
| `sum`/`count`/... | `Fold1 m`/`Fold0 m` | `Semigroup`/`Monoid` |
| 窗口函数 | `Win ...` | 单独的类/grade |

**为什么不能直接复用 Haskell 类：**

- Haskell 的 `Eq a` 的 `(==) :: a -> a -> Bool`；如果 `a = Expr r Int`，
  `(==)` 返回 `Bool`，不是 `Expr r Bool`；SQL 需要后者；
- 同理 `Ord`/`(&&)`/`not`；
- 解决：定义新的 lifted 类，方法返回 `Expr r Bool`；或者显式
  `liftA2 (==)`（但那是"语法"，不是类；无糖原则下可以用函数）。

### 4.3 字面量、默认化、歧义

- `fromInteger`/`fromRational`：`Num`/`Fractional` 实例；`1` 可以多态；
- `IsString`：`"x"` 可以多态到 `Expr r String`；日期/timestamp 字面量
  需要 `fromString` + 期望类型/默认化；
- `Decimal p s`：`fromInteger`/`fromRational` 需要精度/尺度；可能需要
  `Num (Decimal p s)` 或显式 `decimal` 构造；
- 默认化：`default` 机制/类型注解；歧义（`IsString`/`Num`/overloaded）
  需要错误信息；
- `forall`/visible type application：`field @l`、`fromString @...`；
- 无糖：字面量/操作符是类实例；不引入特制 `lit` 语法（除了数字/字符串本身）。

### 4.4 类型类一致性

- 关闭孤儿/重叠实例；行类型是结构性的，`HasField` 每个字段唯一；
- `Expr` 是 newtype，可以有 `Num`/`Semigroup` 等实例；
- lifted 类为每个基础类型提供实例（Int/Float/Decimal/String/Date/...）；
- 用户不能自定义实例（先封闭），避免 incoherence；
- 类定律用 property test（不形式化证明）。

---

## 5. 行类型与推断

### 5.1 行类型

```text
Row ::= { l : A, ... } | { l : A | rho } | rho
```

- 标签 `l : Symbol`；`KnownSymbol`/`Label`；
- 开放行 `{ l : A | rho }`（row polymorphism）；
- 行操作（type families）：`Merge`、`Disjoint`、`Remove`、`KeyMap`、`MapValue`；
- `HasField l r a` 约束；
- join 两行的 merge 要求 `Disjoint`；冲突 = 类型错误；
- `local`/`reindex` 用 row map（类型级，不是宿主函数）。

### 5.2 推断

- HM + row unification；`field @l` 产生 `HasField l r a` 约束；
- `r` 可以是开放行变量；`Expr r a` 的 `r` 由上下文/上下文约束推断；
- 歧义：`HasField` 多个可能、`Num`/`IsString` 默认化；需要注解/默认；
- 显式 `forall r.`/`forall a.`；避免隐式类型变量拼错变成自由变量
  （之前 Cagara 的 P0：`decimal` 拼错静默多态）；
- 未知类型名 = 错误 + did-you-mean；类型变量必须有 binder/约定；
- 错误信息：行不匹配、缺字段/冲突字段、`HasField` 未满足、grade 不匹配；
- 类型别名/行别名：`type Orders = { ... }`；签名可读性；
- 结构行 vs 名义 row type：可选（`newtype` 行别名）。

### 5.3 `HasField` 与 `.field`

- `.field` 是 `field @l` 的语法；`l` 由标签决定；`r`/`a` 推断；
- 开放行：`.field` 在 `{ field : A | rho }` 上合法；
- 闭行：缺失字段 = 类型错误；
- 多个同名字段（join 未限定）应报错；用 row merge/qualified labels 解决；
- `local`/`reindex` 后字段类型按 row map 变化；
- `.field` 不是糖：它是 `HasField` 投影/`Field` primitive；语义是 `asks`。

---

## 6. Phase / grade

### 6.1 类型

```text
data Phase = Row | Agg | Win
Expr (g :: Phase) r a
type family Combine (g :: Phase) (h :: Phase) :: Phase
-- Row ⊗ Row = Row; Row ⊗ Agg = Agg; Agg ⊗ Agg = stuck;
-- Win ⊗ Win = stuck; Agg ⊗ Win = stuck
```

- `sum`/`count` 等只在 `Agg`（或 `Agg` 的表达式上下文）；
- `rowNumber`/`lag` 等只在 `Win`；
- `filter`/`map` 需要 `Row`；
- `Combine` 一张表替代 `rules.rs` 的手写规则；
- `Expr` 是 **graded monad**/`ReaderT` over `Grade`：`Expr g r a = ReaderT r (Grade g ExprM) a`；
- 类型规则：

```text
Γ ⊢ e : Expr Row r a
---------------------  (group)
Γ ⊢ group e : Expr Agg r a

Γ ⊢ e : Expr Agg r a      op : AggOp
--------------------------------------
Γ ⊢ op e : Expr Agg r m

Γ ⊢ e : Expr Win r a
---------------------
Γ ⊢ window e : Expr Win r a
```

### 6.2 与查询层

- `groupBy : Expr Row r k -> Query r (k, Group r)`；
- `agg : (r -> Fold1 m) -> Group k r -> ...`；
- `window : WindowSpec -> Query r (r, w)`；
- `sortOn`/`page` 是 `Ordered`/list，不是 `Expr`；
- 窗口不能嵌套；聚合不能嵌套；filter/map 只能 Row；
- 无糖：phase 是类型索引，不是 `expr/agg/win` 三套语法。

---

## 7. 参数舞台：两个 Reader

### 7.1 类型

```text
Param a          -- 宿主/编译期值
Expr r a         -- 行表达式
Code (Expr r a)  -- quoted expression（LID）
```

- 参数 Reader 与行 Reader 是**两个不同的环境**：
  `ReaderT Param (ReaderT Row ExprM) a`；不要混成一个；
- `Param` 值在归一化时替换；`~p` 反引用；一条 SQL；
- 参数不会与行类型 unification；修"参数经 projection 丢失"和
  "同一参数两次 join 丢一侧"的 bug；
- 类型级标签区分（`ParamEnv` vs `RowEnv`）。

### 7.2 规则

```text
Γ, p : Param A ⊢ e : Expr r B
--------------------------------
Γ ⊢ λp. e : Param A -> Expr r B

Γ ⊢ p : Param A      Γ ⊢ e : Expr r B
--------------------------------------  (antiquote)
Γ ⊢ ~p : Expr r A
```

- `~p` 只能插入 `Param`，不能插入任意宿主值；
- 归一化把 `~p` 变成 SQL 参数/字面量；
- 两阶段类型保证 "one SQL"（在静态片段内）；
- 无糖：`~` 是反引用原语，不是 `sql` 字符串拼接。

---

## 8. NULL 与 totality

### 8.1 NULL

- `Maybe` 在值层：`Expr r (Maybe A)`；
- 谓词要求 `Expr r Bool`；`Maybe Bool` 用 `whereTrue`/`isTrue` 显式降级；
- `eqMaybe`/`isNull`/`coalesce` 是普通函数/类；
- **不要** `MaybeT (ReaderT r m)`：那会让整个表达式可能失败，不是 SQL NULL；
- SQL 3VL 与 `Maybe` 2VL 有差异；要么明确 2VL 边界，要么显式 3VL；
- `Maybe` 的 `Ord` 不能直接用 Haskell `Maybe` 的 `Ord`（Nothing 最小）；
  `Ordered`/`NullsOrder` 显式；
- `sum` 对 `Maybe` 输入 → `Maybe`；`count` 忽略 `Nothing`；`whereTrue`。

### 8.2 totality

- 无 `undefined`/`error`/异常；偏函数返回 `Maybe`/`Either`；
- `safeDiv :: Int -> Int -> Maybe Int`；`NonZero` 精化；`div :: Int -> NonZero Int -> Int`；
- `Nat`（limit/offset/page）；`NonEmpty`（组/分区）；`Decimal p s`；
- `newtype`/ADT（UserId/Money/Status）；`SqlRep` 类；
- `Group`/`Ordered` 类型保证非空/有序；
- 整数溢出：`Integer`/`Decimal` 或 checked `Maybe`；不与引擎语义冲突；
- 归一化 fuel/终止度量；递归单独 `fix`；
- 类型正确性不是"零运行期错误"：DB/数据/引擎错误仍显式（`Maybe`/capability）。

---

## 9. 方言 capability

```haskell
data Dialect = Postgres | DuckDB | SQLite | BigQuery | ...
data Feature = FullJoin | NullsFirst | Decimal | TryCast | ...
class Supports (d :: Dialect) (f :: Feature)

fullJoin :: Supports d 'FullJoin => Query r s -> ...
compile  :: forall d. KnownDialect d => Query () r -> SQL d
```

- 类型级 dialect；未支持 = 类型错误；
- `Expr`/`Query` 不直接依赖 dialect；只有 lowering 阶段需要；
- 避免"能生成 = 能运行"的虚假承诺；
- 无糖：capability 是约束，不是运行期检查。

---

## 10. Deep / Tagless / Shallow 的取舍

| 需求 | Shallow Reader | Deep Reader | Tagless final |
|---|---|---|---|
| `.field` 自动 lift | ✓ `asks` | ✓ `Field` | ✓ `field` |
| 自动 `Num`/`Semigroup` | ✓ | ✓ `App`/类 | ✓ 类 |
| lifted `Eq/Ord/Bool` | 需类/函数 | 需类/primitive | 需类 |
| 可检查 SQL | ✗ | ✓ | 实例可 |
| 归一化/重写 | 需 quotation | ✓ | 弱 |
| 多后端 | quotation | `cata` | ✓ |
| 参数舞台 | ✓ LID | ✓ | ✓ |
| 类型推断 | 简单 | 行+类 | rank-2 |
| 推荐 | 语义/参考 | **编译器核心** | 预置库/API |

**建议：** 核心用 **Deep Reader**；预置库用 **Tagless final**；参考语义/宿主
嵌入用 **Shallow Reader/LID**。三者共享同一组类型类（`ExprLang`），
`ReaderT` 是语义解释；`Field`/`HasField` 是 `.field` 的类型。

---

## 11. 定律（陈述，不证明）

### 11.1 Reader / Expr

```text
ask >>= k          = k ask
local id           = id
local f . local g  = local (g . f)
asks f             = fmap f ask
field @l           = asks (getField @l)
local f (field @l) = field @l . f      -- 类型级 row map 时
```

### 11.2 Applicative / Monad / lift

```text
pure id <*> v = v
pure (.) <*> u <*> v <*> w = u <*> (v <*> w)
liftA2 f x y = f <$> x <*> y
Num/Fractional/Semigroup/Monoid/IsString 实例逐点
LiftEq/LiftOrd/LiftBool 与基础类一致
```

### 11.3 Query

```text
Query 是 indexed monad / category：
pure >>= k = k
q >>= pure = q
(q >>= k) >>= h = q >>= (k >=> h)
filter p (q <> q') = filter p q <> filter p q'
map f (q <> q') = map f q <> map f q'
```

### 11.4 Folds / order / maybe

```text
foldMap1 f (map g q) = foldMap1 (f ∘ g) q
sortOn k (sortOn k q) = sortOn k q
page 的合成律（明确 skip/take 顺序）
Maybe 的 Functor/Monad/Alternative 律
```

### 11.5 类型可靠性（陈述）

- 若 `Γ ⊢ e : Expr r a`，则 `eval e` 是 `r -> a` 的纯函数；
- 若 `Γ ⊢ q : Query r s`，则 `⟦q⟧` 是 `r` 到 `s` 的 bag/关系；
- 行/phase/param/grade 的类型规则保证不会出现运行期"缺列/嵌套/参数丢失"；
- `Maybe`/`NonEmpty`/`NonZero`/`Nat`/`Ordered` 保证 partiality/empty/order 被显式化；
- 不做形式化证明；用 property test + compile-fail test 验证。

---

## 12. 无糖的核心 surface

**允许（核心）：**

- `do`/`>>=`/`fmap`/`pure`（monad/indexed monad）；
- `.field`（`HasField`/`field`，语义 `asks`）；
- `liftA2`/`Num`/`Semigroup`/`Monoid`/`IsString` 实例；
- lifted 类 `LiftEq`/`LiftOrd`/`LiftBool`；
- `groupBy`/`foldMap`/`foldMap1`/`sortOn`/`take`/`page`；
- `ifThenElse`/`case`/`let`/records/application/annotations；
- `Param`/`~` 反引用；
- `sql` 模板（typed holes）作为 escape hatch。

**不允许（糖/第二套写法）：**

- `summarize`/`window`/`select`/`update`/`agg` 等 stage 语法；
- `&?`/`&=`/`&+`/`&*` 等符号简写；
- 为每个 SQL 算子造特制 AST/关键字；
- 隐式 cast/隐式默认化（除类型类默认化）；
- 把 `.field` 做成特殊语法（它只是 `HasField` 投影）；
- SQL 字符串拼接（`sql` 模板除外）。

**`.field` 的地位：** 不是糖，而是 `HasField` 的投影/`Field` primitive；
它的语义是 `ReaderT` 的 `asks`，类型是 `HasField l r a => Expr r a`。
这正是"Reader Monad 隐式自动 lift"的严格版本。

---

## 13. 开放问题（研究议程）

1. **Reader 的可检查性**：Deep Reader 的 `Field`/`Prim`/`Local` 最小集合；
   `local` 的 row map 是类型级还是宿主函数；SQL 解释器如何处理 scope。
2. **lifted `Eq/Ord/Bool`**：自定义类的设计、实例、默认化/歧义；
   是否允许 `liftA2` 作为显式替代；与 Haskell 类的关系。
3. **Query 类型**：`Rel r` monad vs indexed `Query r s`；`do`/`pure`/`map`
   的规则；与 `Rel` 特例/语法糖；`join`/子查询的类型。
4. **行推断**：`HasField` + 开放行的 HM 推断；歧义/默认化；显式 `forall`；
   未知类型名报错；行别名/闭行；错误信息。
5. **phase grade**：`Expr g r a` 的 grade monad；`Combine` 类型族；
   与 `ReaderT`/`free` 的组合；推断/错误。
6. **参数舞台**：两个 Reader（Param/Row）的分离；`~` 反引用；one-SQL；
   与 `Code`/quotation 的关系。
7. **NULL/3VL**：`Maybe` 值层 + 2VL 谓词 vs 完整 3VL；`MaybeT` 的取舍；
   `NOT IN`/`EXISTS`；等价性。
8. **totality/refinement**：`NonEmpty`/`NonZero`/`Nat`/`Decimal` 的类型类/精化
   实现；与 `newtype`/ADT/`SqlRep` 的交互；安全除法/溢出。
9. **dialect capability**：`Supports` 约束/类型级 dialect；与查询类型的交互。
10. **Deep vs Tagless**：归一化/重写/多后端；能否共享同一组类；
    性能/类型推断成本。
11. **无糖的边界**：最小的核心 surface 是什么；`.field`/`do`/lifted 类
    是否已足够；哪些语法是不可避免的（records/literals/if/case）。
12. **类型可靠性/机械化**：Statement + property tests；未来 Coq/Lean；
    与 U-semiring/NRC 语义的连接（上一轮研究）。

---

## 14. 参考

1. **Haskell `ReaderT` / `MonadReader`**（mtl）；`Reader`/`Env` comonad
   （Uustalu–Vene, *The Essence of Dataflow Programming*）。
2. **GHC `HasField` / `GHC.Records`**；`row-types`、`vinyl`、`superrecord`；
   Leijen 的 row polymorphism；Rémy 的 row types。
3. **Kiselyov 等, "Finally tagless, partially evaluated"**——类/多解释器。
4. **Swierstra, "Data types à la carte"**；`Free` monad/initial algebra。
5. **Atkey, parameterized/indexed monads**；**Orchard 等, graded monads**；
   **Petricek 等, coeffects**——phase/grade。
6. **Hughes, "Generalising Monads to Arrows"**；profunctors/lenses。
7. **Cheney, Lindley, Wadler, "A Practical Theory of Language-Integrated
   Query"**——quotation/params/one-SQL。
8. **Liquid Haskell**——精化类型（`NonEmpty`/`NonZero`/`Nat`）。
9. **HaskellDB / Links / Ferry**——typed SQL embedding 的先例。
10. **Cagara 现有文档**：`docs/CORE.md`、`docs/ROW-TYPES.md`、
    `docs/NULLABILITY.md`——HM + 行类型 + 可空性的现状。

---

## 15. 一句话总结

> **`.field` 的 ReaderT 视角是对的：`Expr r a` 是行环境上的 reader，
> `.field = asks`，同结果类型的算子靠 `Applicative`/类型类自动 lift；
> 但 `Eq/Ord/Bool` 要 lifted 类，且 ReaderT 只是语义——SQL 生成需要
> Deep Reader（`Field` primitive）或 Tagless 实例。**
>
> 类型系统的核心应该是：**row types + `HasField` + Reader-indexed
> expressions + indexed `Query r s` + phase grades + two-level params +
> `Maybe`/totality + dialect capabilities**；表面只用 `do`/`fmap`/`field`/
> `liftA2`/`groupBy`/`foldMap`/`sortOn`/`take`，不造第二套糖。