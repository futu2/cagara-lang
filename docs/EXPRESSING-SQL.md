# 如何表达 SQL 的逻辑：一个带例子的设计研究

> 这一轮不做形式化证明。目标是把上一轮的理论落成**可写的表达式**：
> 逐个 SQL 构造，给出"用 Haskell 风格怎么表达"的实例，观察哪些自然、
> 哪些别扭、哪些暴露出理论边界。
>
> 结论先行：**一条查询的表达骨架是四层管道**
>
> ```text
> Comprehension / do        -- 关系层：FROM/WHERE/SELECT/JOIN
>   & Summarize / Group     -- 折叠层：GROUP BY + 聚合
>   & Window                -- 呈现层：窗口/有序计算
>   & Sort / Page           -- 呈现层：ORDER BY / LIMIT / OFFSET
> ```
>
> 每一层对应不同的代数结构（monad / monoid / comonad / list），
> 层与层之间的类型不同，所以不能混成一组 SQL 关键字。

---

## 0. 约定（语法是探索性的）

本文用一套 Haskell 风格、接近 Cagara 的伪语法，重点在表达式结构，
不在最终关键字：

```haskell
-- 表：带行类型
orders : Rel { id = int, user_id = int, amount = Decimal 18 2,
               status = string, created_at = date }
orders = table "public" "orders"

-- 关系层：do / comprehension
paid = do
  o <- orders
  guard (o.status == "paid")
  pure { user_id = o.user_id, amount = o.amount }

-- 管道
report =
  paid
  & summarize (.user_id) { total = sum .amount, n = count }
  & filter (.n >= 5)
  & sortOn (desc .total)
  & page 0 10
```

- `Rel r`：bag of row `r`（对应 Cagara 的 `query`）；
- `groupBy`/`summarize`：分组 + 折叠；
- `summarize key { ... }` 是 `groupBy` + `foldMap1` 的糖；
- `page skip take`：唯一的分页算子（没有 `limit` 后接 `offset`）；
- `window`：窗口层；
- `sortOn`：返回 `Ordered`（只有它能被 `page` 消费）；
- `Maybe`：SQL NULL 的值层表示；
- `summarize`/`window`/`page` 都是管道 stage，不是 SQL 关键字。

---

## 1. SQL → 表达式速查表

| SQL | 表达式（核心） | 表达式（糖） |
|---|---|---|
| `SELECT f(x) FROM t` | `t & fmap f` / `do x <- t; pure (f x)` | — |
| `WHERE p` | `filter p` / `guard (p x)` | — |
| `FROM a, b WHERE p` | `do a' <- a; b' <- b; guard (p a' b'); pure ...` | — |
| `A JOIN B ON p` | 同上（内连接 = 多生成器 + guard） | — |
| `LEFT JOIN` | `leftJoin` + `Maybe` 右行 | — |
| `EXISTS` | `semiJoin` / `any` | — |
| `NOT EXISTS` | `antiJoin` | — |
| `IN (list)` | `elem` / `inList` | — |
| `IN (subquery)` | `semiJoin` | — |
| `GROUP BY k` | `groupBy k` | — |
| `SUM x` | `foldMap1 (Sum . x)` | `sum .x` |
| `COUNT *` | `foldMap1 (const (Count 1))` | `count` |
| `COUNT x` | `foldMap1 (Count . isJust . x)` / `countOf` | `countOf .x` |
| `COUNT(DISTINCT x)` | `foldMap1 (Set.singleton . x)` + `size` | `countDistinct .x` |
| `AVG x` | `(Sum, Count)` 半群 + 除法 | `avg .x` |
| `MIN/MAX` | `Min`/`Max` 半群 | `min`/`max` |
| `SUM(CASE WHEN p THEN x END)` | 条件 `Semigroup`/`Maybe` | `sumIf p .x` |
| `HAVING p` | `summarize` 后 `filter p` | — |
| `UNION ALL` | `<>`（bag 加法） | — |
| `UNION` | `distinct . (<>)` | — |
| `DISTINCT` | `distinct` | — |
| `INTERSECT/EXCEPT` | 集合层 `intersect`/`except` | — |
| `ORDER BY` | `sortOn`（→ `Ordered`） | — |
| `LIMIT/OFFSET` | `page` | — |
| 窗口函数 | `window` + `rowNumber`/`lag`/`sum` | — |
| CTE / 复用 | `let` / 定义共享 | — |
| 递归 CTE | `fix`（独立层，核心之外） | — |

---

## 2. 关系层：`do` 就是 comprehension

### 例 1：过滤、投影、排序、分页

SQL：

```sql
SELECT id, UPPER(name) AS name, age + 1 AS next_age
FROM users
WHERE age >= 18 AND active
ORDER BY name ASC
LIMIT 10;
```

表达式：

```haskell
adults =
  do
    u <- users
    guard (u.age >= 18 && u.active)
    pure { id = u.id, name = upper u.name, next_age = u.age + 1 }
  & sortOn (asc .name)
  & page 0 10
```

或者无 `do` 的管道：

```haskell
adults =
  users
  & filter (\u -> u.age >= 18 && u.active)
  & fmap (\u -> { id = u.id, name = upper u.name, next_age = u.age + 1 })
  & sortOn (asc .name)
  & page 0 10
```

**观察：**

- `do`/`guard`/`pure` 与 `filter`/`fmap` 是同一件事（comprehension）；
  Haskell 程序员两种都熟。
- `sortOn`/`page` 在关系层**之外**：`sortOn` 返回 `Ordered`，`page` 只接受
  `Ordered`。没有"未排序就分页"，也没有"先 `limit` 再 `offset`"的陷阱。
- 类型：`u : { id = int, ... }`；`pure` 构造输出行；`sortOn` 要求
  `Ord key`；`page` 的两个参数是 `Nat`。

### 例 2：三表内连接（同名问题自然消失）

SQL：

```sql
SELECT o.id AS order_id, u.name AS user_name, m.name AS merchant_name
FROM orders o
JOIN users u ON o.user_id = u.id
JOIN merchants m ON o.merchant_id = m.id
WHERE o.status = 'paid';
```

表达式：

```haskell
order_names =
  do
    o <- orders
    u <- users
    m <- merchants
    guard (o.user_id == u.id)
    guard (o.merchant_id == m.id)
    guard (o.status == "paid")
    pure { order_id = o.id, user_name = u.name, merchant_name = m.name }
```

**观察：**

- 内连接就是"多个生成器 + guard"。**列引用由变量限定**，`u.name` 和
  `m.name` 不可能混淆，也不需要 `.<x`/`.>x`。
- 自连接同样自然：`a <- employees; b <- employees; guard (a.manager_id == b.id)`。
- 归一化把 `do` 变成 `FROM/JOIN/WHERE`；join 顺序由优化器/代价决定。
- 这就是之前 P0"join 同名列静默丢右侧"的类型级解：**变量限定 + 输出行显式构造**，
  没有"左胜"规则可丢。
- 代价：显式写 `guard` 比 SQL 的 `ON` 多几行；但对 Haskell mind model 是自然的。

### 例 3：外连接与 `Maybe`

SQL：

```sql
SELECT u.id, o.id AS order_id, o.amount
FROM users u
LEFT JOIN orders o ON u.id = o.user_id;
```

表达式（一种设计）：

```haskell
user_orders =
  leftJoin orders (\u o -> u.id == o.user_id) users
  -- 结果行：{ left = users, right = Maybe orders }
  & fmap \{ l, r } ->
      { user_id = l.id
      , order_id = r <&> .id       -- Maybe int
      , amount   = r <&> .amount   -- Maybe (Decimal 18 2)
      }
```

**观察：**

- 外连接打破 `do` 的对称性：右行是 `Maybe`，所以右侧字段也是 `Maybe`。
- 两种输出模型：
  1. **嵌套**：`{ left, right : Maybe row }`；无列名冲突；Haskell 自然；
  2. **摊平**：右侧字段变 `Maybe`，但需要类型级 `Disjoint` 检查（否则
     `id` 冲突）。
- `Maybe` 字段访问的语法是待定问题：`r <&> .amount`、`r.amount : Maybe`、
  或者一个 null-aware 的 `.?.`。这层语法决定外连接写起来是否顺手。
- 这里已经能看出：**NULL 不是 `Maybe` 的全部**——SQL 的 `WHERE` 对
  `NULL` 谓词是 unknown；所以 `filter` 要求 `Bool`，需要 `whereTrue`/`isTrue`
  显式降级（见 §5）。

---

## 3. 折叠层：`GROUP BY` + 聚合 = `foldMap1`

### 例 4：SUM / COUNT / AVG / HAVING / Top-N

SQL：

```sql
SELECT user_id, SUM(amount) AS total, COUNT(*) AS n, AVG(amount) AS avg
FROM orders
WHERE status = 'paid'
GROUP BY user_id
HAVING COUNT(*) >= 5
ORDER BY total DESC
LIMIT 10;
```

表达式（糖）：

```haskell
revenue =
  orders
  & filter (.status == "paid")
  & summarize (.user_id)
      { total = sum .amount
      , n     = count
      , avg   = avg .amount
      }
  & filter (.n >= 5)
  & sortOn (desc .total)
  & page 0 10
```

核心展开（`summarize` 的语义）：

```haskell
revenue =
  orders
  & filter (.status == "paid")
  & groupBy (.user_id)                          -- Map UserId (NonEmpty Row)
  & fmap (foldMap1 (\o ->
      { total = Sum o.amount                     -- Semigroup
      , n     = Count 1                          -- Semigroup
      , avg   = (Sum o.amount, Count 1)          -- Semigroup，之后相除
      }))
  & toRel                                        -- Map -> Rel
  & filter (.n >= 5)
  & sortOn (desc .total)
  & page 0 10
```

**观察：**

- **聚合 = 对非空组做 `foldMap1`**。`sum`/`count`/`avg` 不是特殊语法，而是
  `Semigroup` 值；`summarize` 只是把 key 和多个 fold 组合成一条记录。
- 组非空（SQL 的 `GROUP BY` 不会产生空组），所以 `sum`/`avg` **非空**，
  不需要 `coalesce 0.0 (sum .amount)`；`avg` 用 `(Sum, Count)` 半群，
  非空组天然非空。
- `HAVING` 就是 `summarize` 后的 `filter`；SQL 形状可以选 `HAVING` 或派生表，
  两者等价（同一表达式，不同 presentation）。
- `page` 只接 `Ordered`，`sortOn` 已经提供；顺序/分页语义显式。
- 这一段是整套表达的核心：**关系层用 monad，聚合层用 monoid**；两层之间
  只有 `groupBy` 一个接口。

### 例 5：条件聚合、`COUNT(DISTINCT)`、`FILTER`

SQL：

```sql
SELECT user_id,
       SUM(CASE WHEN status = 'refund' THEN amount ELSE 0 END) AS refunded,
       COUNT(DISTINCT status) AS statuses,
       COUNT(*) FILTER (WHERE amount > 100) AS big
FROM orders
GROUP BY user_id;
```

表达式：

```haskell
& summarize (.user_id)
    { refunded = sumIf (.status == "refund") .amount
    , statuses = countDistinct .status
    , big      = countIf (.amount > 100)
    }
```

核心：

```haskell
& groupBy (.user_id)
& fmap (foldMap1 (\o ->
    { refunded = if o.status == "refund" then Sum o.amount else mempty
    , statuses = Set.singleton o.status
    , big      = if o.amount > 100 then Count 1 else mempty
    }))
```

**观察：**

- `SUM(CASE WHEN p THEN x ELSE 0 END)` = 条件 `Semigroup`；
  `SUM(CASE WHEN p THEN x END)`（无 ELSE，SQL 为 NULL）= 条件
  `Maybe`/`fold0`，两者语义不同，类型不同。
- `COUNT(DISTINCT x)` = `Set` 半群 + `size`；`COUNT(DISTINCT)` 忽略 NULL，
  所以要用非空值/`just` 后的 `Set`；这是"半群选择 = 语义选择"的典型例子。
- `FILTER (WHERE p)` 不是新语法，就是条件半群/`foldMap1`。
- **不是所有聚合都是半群**：`median`/`percentile`/`mode` 需要 `Collect`
  （list monoid）再后处理，或走 `sql` 模板；这是表达层的边界，见 §8。

### 例 6：`GROUP BY` 多 key / `GROUPING SETS`（硬例子）

SQL：

```sql
SELECT region, product, SUM(amount) AS total
FROM sales
GROUP BY GROUPING SETS ((region, product), (region), ());
```

表达式（一种写法）：

```haskell
by_region_product =
  sales & summarize (.region, .product) { total = sum .amount }

by_region =
  sales & summarize (.region) { total = sum .amount }
        & fmap (\r -> { r | product = Nothing })

grand =
  sales & summarize () { total = sum .amount }
        & fmap (\r -> { r | region = Nothing, product = Nothing })

all_sets = by_region_product <> by_region <> grand
```

**观察：**

- `GROUPING SETS` 不是一种新的分组，而是**多个 `groupBy` 的 union**，
  缺少的 key 用 `Nothing` 填充；类型上 key 变成 `Maybe`。
- 这种写法可行，但"补 Nothing + union"是手工的；可以做一个
  `rollup`/`groupingSets` 组合子，把它变成类型化的组合。
- 这是 SQL 的"聚合形状"里最不优雅的一块；理论上是 `groupBy` 的 coproduct，
  实践中可能需要糖。

### 例 7：左连接 + 聚合 + NULL

SQL：

```sql
SELECT u.id,
       COALESCE(SUM(o.amount), 0) AS total,
       COUNT(o.id) AS n
FROM users u
LEFT JOIN orders o ON u.id = o.user_id
GROUP BY u.id;
```

表达式：

```haskell
user_totals =
  leftJoin orders (\u o -> u.id == o.user_id) users
  & fmap \{ l, r } ->
      { user_id = l.id
      , amount  = r <&> .amount     -- Maybe Decimal
      , order_id = r <&> .id        -- Maybe int
      }
  & summarize (.user_id)
      { total = coalesce 0 (sum .amount)      -- 语义 1：NULL 当 0
      , n     = countOfMaybe (.order_id)      -- 语义 2：只数匹配行
      }
```

**观察：**

- 同一个"左连接 + 聚合"有两种常见语义：
  1. `SUM(COALESCE(amount, 0))`：没匹配当作 0；
  2. `SUM(amount)`：没匹配是 NULL；`COALESCE 0` 只在最后。
- 类型能区分它们：`sum .amount` 输入是 `Maybe`，结果也是 `Maybe`；
  `coalesce 0` 把它变成非空；`countOfMaybe` 只数 `Just`。
  这正是 `Fold1`/`Fold0` 的用武之地。
- 表达式比 SQL 长，因为外连接的 `Maybe` 必须显式摊平；但语义是清楚的。
- 如果 `leftJoin` 直接摊平右侧字段（冲突类型报错），写法会短一些；
  这是 API/语法决策，见 §9。

---

## 4. 集合/子查询层

### 例 8：`UNION ALL` vs `UNION` vs `DISTINCT`

SQL：

```sql
SELECT id, email FROM customers
UNION ALL
SELECT id, email FROM suppliers;

SELECT DISTINCT id, email FROM (
  SELECT id, email FROM customers
  UNION
  SELECT id, email FROM suppliers
) t;
```

表达式：

```haskell
all_contacts =
  (customers & fmap (\c -> { id = c.id, email = c.email }))
  <> (suppliers & fmap (\s -> { id = s.id, email = s.email }))

-- UNION ALL：bag 加法，保留重复
unique = distinct all_contacts
```

**观察：**

- `<>` 是 `Semigroup`（bag union），`distinct` 是商到集合；`UNION` =
  `distinct . (<>)`。两种语义显式，不会把 `UNION`/`UNION ALL` 搞混。
- 行的名字/顺序必须一致；类型级 `SameRow` 检查；不同顺序可以自动按名对齐
  （设计选择）或者报错。
- `distinct` 不是半环同态，不能随意下推/重排；这是它单独一层的原因。

### 例 9：`EXISTS`、`NOT EXISTS`、`IN`

SQL：

```sql
-- 有订单的客户
SELECT c.id FROM customers c
WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id);

-- 没有订单的客户
SELECT c.id FROM customers c
WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id);

-- IN 列表
SELECT id FROM customers WHERE region IN ('EU', 'APAC');
```

表达式：

```haskell
with_orders =
  customers & semiJoin orders (\c o -> c.id == o.customer_id)

without_orders =
  customers & antiJoin orders (\c o -> c.id == o.customer_id)

eu_apac =
  customers & filter (\c -> c.region `elem` ["EU", "APAC"])
```

**观察：**

- `EXISTS`/`NOT EXISTS` = semijoin/antijoin；`IN (subquery)` = semijoin；
  `IN (list)` = `elem`/`inList`。它们都有类型（`semiJoin` 保留左行，
  `antiJoin` 保留无匹配左行）。
- `NOT IN` 的 SQL 三值逻辑是陷阱：子查询里有 NULL 时，`NOT IN` 可能一行都不返回。
  `antiJoin` 的语义是 `NOT EXISTS`（不是 `NOT IN`）；如果要精确 `NOT IN`，
  需要显式处理 `NULL`。这正是"为什么不能把 3VL 藏起来"的例子。
- 泛化的相关子查询（`EXISTS` 里带复杂聚合/多层相关）超出 semijoin 能表达的范围；
  核心层可以拒绝或降级到 `sql` 模板；或者用 `let`/子查询组合。

### 例 10：相关标量子查询

SQL：

```sql
SELECT c.id,
       (SELECT MAX(o.amount) FROM orders o WHERE o.customer_id = c.id) AS max_amount
FROM customers c;
```

表达式：

```haskell
customer_max =
  leftJoin orders (\c o -> c.id == o.customer_id) customers
  & fmap \{ l, r } -> { id = l.id, amount = r <&> .amount }
  & summarize (.id) { max_amount = max .amount }   -- Maybe，未匹配为 NULL
```

**观察：**

- 相关标量子查询 = 左连接 + 分组 + 聚合；SQL 可能 decorrelate 成 join/group，
  也可能逐行执行；表达式层面就是后者语义。
- 结果类型 `Maybe`（未匹配 = NULL），与 SQL 标量子查询一致。
- 相关的 `COUNT/SUM`、`EXISTS` 都可以用同样模式；`LATERAL` 这种"右表依赖左表
  整行"的任意相关查询是边界，核心演算可以拒绝。

---

## 5. 呈现层：窗口与有序

### 例 11：Top-N per group

SQL：

```sql
SELECT id, user_id, amount, rn
FROM (
  SELECT id, user_id, amount,
         ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY amount DESC) AS rn
  FROM orders
) t
WHERE rn <= 3;
```

表达式：

```haskell
top3 =
  orders
  & window (partitionBy (.user_id)) (orderBy (desc .amount))
      { rn = rowNumber }
  & filter (.rn <= 3)
```

**观察：**

- 窗口是**关系层之外的另一个阶段**：`window` 产出带窗口列的行，
  然后 `filter`；不能在同一个 stage 里既算窗口又过滤（SQL 也不允许）。
  这就是 phase/grade 的意义。
- `window` 的类型可以要求 `Ordered`/分区结构；`rowNumber` 是 comonadic
  位置操作；`rank`/`denseRank` 同类。
- SQL 需要子查询；表达式只需要两个 stage；归一化决定 SQL 形状。

### 例 12：running sum、lag、占比

SQL：

```sql
SELECT id, user_id, amount,
       SUM(amount) OVER (PARTITION BY user_id ORDER BY created_at
                         ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running,
       LAG(amount) OVER (PARTITION BY user_id ORDER BY created_at) AS prev,
       amount / SUM(amount) OVER (PARTITION BY user_id) AS share
FROM orders;
```

表达式：

```haskell
running =
  orders
  & window (partitionBy (.user_id)) (orderBy (asc .created_at))
      { running = sum .amount over runningFrame
      , prev    = lag 1 .amount
      , total   = sum .amount over wholePartition
      }
  & fmap (\o -> { o | share = o.amount / o.total })
```

**观察：**

- `runningFrame`/`wholePartition` 是 frame 值；`sum` 在窗口里的类型是
  `WindowFold`，帧可能为空 → `Maybe`；`wholePartition` 非空 →
  可以给非空类型（如果分区非空）。
- `lag` 在分区边缘返回 `Maybe`；`share` 计算要处理 `Nothing`/0。
- 窗口里也可以有 `count`/`avg`；窗口和聚合是同一套 `Semigroup` 值，
  只是作用在"帧"而不是"组"上——这是很漂亮的统一。
- 窗口不能嵌套；可以分多个 stage 算。

### 例 13：`DISTINCT ON` / 每组第一行

SQL：

```sql
SELECT DISTINCT ON (user_id) *
FROM orders
ORDER BY user_id, created_at DESC;
```

表达式：

```haskell
latest =
  orders
  & window (partitionBy (.user_id)) (orderBy (desc .created_at))
      { rn = rowNumber }
  & filter (.rn == 1)
```

**观察：**

- `DISTINCT ON` 本质上是"按某个顺序取每组第一行"，是窗口/有序层的组合，
  不是关系层的 `distinct`。
- 表达式直接表达语义；SQL 的 `DISTINCT ON` 是语法糖/方言特性，归一化时可以
  选择生成 `DISTINCT ON`（Postgres）或窗口子查询（其他方言）。

### 例 14：分页与 NULL 排序

SQL：

```sql
SELECT id, name FROM users
ORDER BY name ASC NULLS FIRST, id DESC
LIMIT 20 OFFSET 40;
```

表达式：

```haskell
page2 =
  users
  & sortOn (ascNullsFirst .name, desc .id)
  & page 40 20
```

**观察：**

- `page skip take` 一个算子；不存在"先 limit 再 offset"。
- `ascNullsFirst` 是显式排序键，不依赖 Haskell `Maybe` 的 `Ord`（那会把
  `Nothing` 排最前，和 SQL 的 NULL 语义不一定一致）。
- `page` 只能在 `Ordered` 上用；没排序的分页是类型错误。

---

## 6. 复用、CTE、参数

### 例 15：CTE / 重复子查询

SQL：

```sql
WITH paid AS (
  SELECT * FROM orders WHERE status = 'paid'
)
SELECT p.user_id, COUNT(*) AS n
FROM paid p JOIN paid q ON p.user_id = q.user_id
GROUP BY p.user_id;
```

表达式：

```haskell
paid = orders & filter (.status == "paid")

report =
  do
    p <- paid
    q <- paid
    guard (p.user_id == q.user_id)
    pure { user_id = p.user_id, n = 1 }
  & summarize (.user_id) { n = count }
```

**观察：**

- 查询是值，`paid` 可以复用；归一化决定内联还是 CTE（共享子树）。
- 不需要专门的 `WITH` 语法；`let`/顶层定义就是 CTE。
- 共享的语义是 bag/重复的；如果 `paid` 被用两次，SQL 的 CTE 可能物化，
  也可能内联；这是 presentation/性能问题，不是语义问题。

### 例 16：参数化查询（修真实工程的 bug）

真实工程里曾经这样写（参数经过 projection 后丢失）：

```cagara
f = pd => a & where (.k == pd) & select { k2 = .k } & where (.k2 >= pd)
# error: no column `k2`; available: k, status
```

在两级/staged 设计里：

```haskell
f : Param Date -> Rel { k : string, k2 : string }
f d =
  do
    x <- a
    guard (x.k == ~d)          -- ~d：把宿主参数插入查询
    pure { k = x.k, k2 = x.k }
  & filter (.k2 >= ~d)
```

**观察：**

- `d : Param Date` 是宿主值，不是行表达式；它不会和行类型做 unification，
  所以经过 projection 后仍然可见；同一个参数出现在 join 两侧也不会丢行。
- `~d` 是 LID 风格的反引用/反引用占位；归一化后是 SQL 参数或字面量，
  一条 SQL。
- 这是类型化舞台（two-level）的直接收益；也是之前真实工程里"参数化查询
  塑造了整个代码结构"的根因。

---

## 7. 值域：日期、Decimal、NULL

### 例 17：按月分组（避免 `EXTRACT(MONTH)` 的跨年 bug）

SQL：

```sql
SELECT DATE_TRUNC('month', created_at) AS month, COUNT(*) AS n
FROM orders
GROUP BY 1
ORDER BY 1;
```

表达式：

```haskell
monthly =
  orders
  & summarize (dateTrunc Month .created_at) { n = count }
  & sortOn (asc .month)
```

**观察：**

- `month` 返回 `int`，按它分组会把不同年份的同月合并；`dateTrunc Month`
  返回 `Date`，类型阻止这个错误。
- `dateTrunc` 的 `Month` 是一个单位（可以是 ADT/类型级），不是魔术字符串。

### 例 18：金额（Decimal）

SQL：

```sql
SELECT user_id, SUM(amount) AS total
FROM payments
GROUP BY user_id;
```

表达式：

```haskell
payments : Rel { user_id = int, amount = Decimal 18 2 }
& summarize (.user_id) { total = sum .amount }
```

**观察：**

- `Decimal 18 2` 是类型；`sum` 是 `Semigroup`；`float` 不能静默混入。
- SQL 后端生成 `DECIMAL(18,2)`；如果引擎/方言不支持，类型级 capability
  报错（见 §9）。

### 例 19：NULL 语义

SQL：

```sql
SELECT id FROM t WHERE x = 1;        -- NULL x 不匹配
SELECT id FROM t WHERE x IS NULL;
SELECT id FROM t WHERE NOT (x = 1);  -- NULL x 仍不匹配
```

表达式：

```haskell
-- x : Maybe int
eq_one = t & filter (\r -> r.x == just 1)      -- 只有 Just 1
is_null = t & filter (isNothing .x)
-- 若想要 SQL 的 3VL "NOT (x=1)"：
not_one = t & filter (whereTrue (\r -> not (r.x == just 1)))  -- 显式
```

**观察：**

- `filter` 要求 `Bool`，所以 `Maybe` 比较必须显式：`r.x == just 1` 是 `Bool`
  （`eqMaybe`），不是 unknown；`NOT` 后仍然只匹配非 NULL 的 1，语义和 SQL
  `WHERE NOT (x = 1)` 对 NULL 的行为不同。Cagara 需要明确选哪一种；
  用 `whereTrue`/`isTrue` 或 `eqMaybe` 表达。
- 这是"2VL 边界"的设计：值层 `Maybe`，谓词层 2VL；和 SQL 的完整 3VL
  有差异，但更可组合。要忠实 SQL，则谓词层要 3VL。
- 例子说明：**NULL 不是简单的 `Maybe`**；它和 `WHERE`/`NOT IN`/`EXISTS`
  的交互需要单独设计。

---

## 8. 边界与硬例子

### 8.1 `median`/`percentile`：不是半群

```sql
SELECT user_id, PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY amount) AS median
FROM orders GROUP BY user_id;
```

表达式（一种写法）：

```haskell
& summarize (.user_id) { amounts = Collect .amount }   -- list monoid
& fmap (\g -> { g | median = percentile 0.5 g.amounts })
```

**观察：**

- `median` 不是 `foldMap1` 能直接算的半群；需要 `Collect`（list）再后处理，
  或者 `sql` 模板/窗口 `percentile`。
- 这是"聚合 = 半群"的边界：大部分聚合是半群，但不是全部。类型上要区分
  `Fold1`（可折叠）和 `PostProcess`（收集后计算）。

### 8.2 递归 CTE

```sql
WITH RECURSIVE org AS (
  SELECT id, manager_id, 0 AS depth FROM employees WHERE manager_id IS NULL
  UNION ALL
  SELECT e.id, e.manager_id, o.depth + 1
  FROM employees e JOIN org o ON e.manager_id = o.id
)
SELECT * FROM org;
```

表达式：

```haskell
org = fix \self ->
  (employees & filter (isNothing .manager_id)
             & fmap (\e -> { e | depth = 0 }))
  <> (do
        e <- employees
        o <- self
        guard (e.manager_id == just o.id)
        pure { e | depth = o.depth + 1 })
```

**观察：**

- 递归是独立的 `fix` 层；`SQL-λ` 核心不含它；语义是最小不动点，
  需要单调/终止性；SQL 递归 CTE 是其一个后端。
- 不能把递归塞进 `do`/monoid；它需要单独的类型规则和语义。

### 8.3 `GROUPING SETS` / `ROLLUP`

见例 6：多个 `summarize` + union + `Nothing` key；可以做糖，但类型上
key 变 `Maybe`，且 `GROUPING()` 指示列另说。

### 8.4 `LATERAL` / 任意相关子查询

- semijoin/antijoin/标量聚合覆盖常见相关子查询；
- `LATERAL`（右表引用左表整行、且每行不同）超出"关系核 + fold"；
  核心可以拒绝或走 `sql` escape hatch；
- 这是"关系层 + 集合层"的边界，不必强行表达。

### 8.5 窗口帧/命名窗口

- 帧（`ROWS BETWEEN ...`）是 comonadic 结构的一部分；命名窗口可做 `let`
  共享；
- 帧可以为空 → `Maybe`；`wholePartition` 非空 → 非空；
- 窗口不能嵌套；多个 stage。

### 8.6 方言差异

- `FULL JOIN`（MySQL 不支持）、`NULLS FIRST`（方言差异）、`DECIMAL`
  精度、整数除法、日期函数；
- 类型级 capability：`Supports d feature`；未支持 = 类型错误；
- 同一表达式在不同方言下生成不同 SQL，但语义由 capability 约束。

---

## 9. 从例子里读出的设计结论

### 9.1 最自然的表达

1. **关系层用 `do`/comprehension**：过滤、投影、内连接、自连接、
   半连接都可以用"生成器 + guard + pure"统一表达；
   变量限定天然解决列名冲突。
2. **聚合层用 `groupBy` + `foldMap1`**：`sum/count/avg/min/max/
   countDistinct/sumIf` 都是 `Semigroup` 值；`summarize` 是糖；
   `HAVING` = `filter`。
3. **顺序/窗口用独立的 ordered/comonad 层**：`window` + `sortOn` + `page`；
   窗口/顺序不能混进关系层；分页只有一个算子。
4. **集合层显式**：`<>`（bag）与 `distinct`（set）分开；`UNION` =
   `distinct . <>`；`EXCEPT`/`INTERSECT` 单独。
5. **复用/参数用 `let`/`Param`**：CTE 不需要专门语法；参数用两级舞台。
6. **类型层**：`NonEmpty` 组、`Ordered`、`Nat`、`Decimal`、`Maybe`、
   `Disjoint`/`Merge`、dialect capability。

### 9.2 最别扭的地方

1. **外连接 + 摊平 + NULL**：`Maybe` 传播让记录字段变成 `Maybe`，
   `sum`/`count` 的语义要靠 `Maybe` 半群选择；语法上需要 `.?`/`<&>` 或
   `flattenLeft` 之类，待设计。
2. **`GROUPING SETS`/`ROLLUP`**：多个 `summarize` + `Nothing` + union，
   手工；需要组合子/糖。
3. **非半群聚合**（median/percentile）：`Collect` + 后处理或 SQL escape；
   需要区分 `Fold` 与 `PostProcess`。
4. **递归**：`fix` 独立层；终止性/单调性需要规则。
5. **任意相关子查询/LATERAL**：核心边界；拒绝或 escape。
6. **3VL**：`Maybe` + 2VL 谓词是选择，不是完整 SQL；要明确片段与等价性。
7. **SQL 形状**：HAVING vs 派生表、CTE vs 内联、DISTINCT ON vs 窗口；
   同一表达式多种 SQL，需要 shape pass/代价。

### 9.3 表达骨架（建议）

```text
Query r
  = Comprehension/do        : Rel r          -- SPJ/内连接
  & GroupBy/Summarize       : Rel (k, m)     -- 聚合
  & Window                  : Rel (r, w)     -- 窗口/有序
  & SortOn/Page             : Ordered r      -- 呈现
  & Distinct / Set          : 商层
  & Maybe / LeftJoin        : 值层/外连接
```

- 每一层是不同类型、不同代数；
- `do` 块里不要有聚合；`summarize` 里不要有窗口；`window` 里不要有排序分页；
- 这条约束同时是类型规则（phase/grade）和可读性规则。

### 9.4 一个可能的完整 surface

```haskell
report =
  do
    o <- orders
    u <- users
    guard (o.user_id == u.id)
    guard (o.status == "paid")
    pure { user_id = o.user_id, amount = o.amount, region = u.region }
  & summarize (.user_id, .region)
      { total = sum .amount
      , n     = count
      }
  & filter (.n >= 5)
  & window (partitionBy (.region)) (orderBy (desc .total))
      { rank = rank }
  & filter (.rank <= 10)
  & sortOn (asc .region, desc .total)
  & page 0 100
```

- 读法是：先做关系（join/filter/project），再分组聚合，再窗口排名，
  最后排序分页；
- 每一段都能对应到 SQL，但不需要写 SQL 的嵌套子查询；
- 类型：`sum/count` 非空（组非空），`rank` 是 `int`，`page` 要求
  `Ordered`；`Maybe` 只在外连接/NULL 处出现。

---

## 10. 待研究/待定的问题

1. **外连接的输出模型**：嵌套 `Maybe row` vs 摊平 + `Disjoint`；字段访问
   `r.amount : Maybe` 还是 `r <&> .amount`；如何保持可读。
2. **聚合的 surface**：`summarize` 糖 vs 纯 `groupBy` + `foldMap1`；
   多聚合、条件聚合、`FILTER` 的类型；`Fold1`/`Fold0` 如何暴露。
3. **非半群聚合**：`median/percentile/mode/string_agg` 的类型/API；
   `Collect` + 后处理 vs SQL 模板。
4. **`GROUPING SETS`/`ROLLUP`**：类型化组合子；key 的 `Maybe` 表示；
   `GROUPING()` 指示列。
5. **`DISTINCT ON`/top-1**：窗口 vs `groupBy` + ordered first；
   方言 lowering。
6. **窗口帧/命名窗口**：comonad/Frame 的类型；空帧 `Maybe`；分区非空。
7. **3VL**：2VL 边界 vs 完整 3VL；`NOT IN`/`EXISTS` 的语义；等价性。
8. **递归**：`fix` 的类型/终止/单调；与 SQL 递归 CTE 的对应。
9. **相关子查询/LATERAL**：核心表达 vs escape hatch；decorrelation。
10. **SQL 形状**：HAVING vs 派生表、CTE vs 内联、DISTINCT ON vs 窗口、
    join 顺序；代价模型与 presentation。
11. **类型层**：`Group`/`NonEmpty`/`Ordered`/`Nat`/`Decimal`/`Disjoint`/
    dialect capability；与 Haskell 类型类的映射。
12. **等价性**：U-semiring/fold/comonad 律；SMT/差分；哪些片段可证。
13. **Surface 的 Haskell 性**：`do`/comprehension 与管道 `&` 的混合；
    `fmap`/`filter`/`<>`/`foldMap` 的命名；是否暴露 `Monad`/`Monoid`。
14. **性能**：`do` join 的顺序/去重；free monad 归一化；CTE 决策。

---

## 11. 一句话总结

> 用 Haskell 表达 SQL，最自然的不是"SQL 关键字 DSL"，而是：
>
> **`do`/comprehension 做关系（join/filter/project）→
> `groupBy`/`foldMap1` 做聚合（monoid）→
> `window` 做窗口（comonad）→ `sortOn`/`page` 做呈现（list）；
> 集合/`distinct` 显式；外连接用 `Maybe`；参数用两级舞台。**
>
> 例子表明：大部分 SQL（SPJ、常见聚合、top-N、set ops、exists）都能
> 干净地表达；真正的硬点是外连接 + NULL、非半群聚合、`GROUPING SETS`、
> 递归、`LATERAL`、3VL、方言——这些应该显式分层/列边界，而不是塞进一个
> 万能 `select` 语法。