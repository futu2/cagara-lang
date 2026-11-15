# 纯函数式 SQL 演算：一条更优雅的理论主线

> 目标：在保持 pure FP 的前提下，找一套**能表达 SQL 的静态语义**，不追求全部 SQL，
> 只要求能生成逻辑等价的 SQL。本文给出候选理论的对比、推荐的正交分解，以及
> 它如何映射回 Cagara 的现有实现。

---

## 0. TL;DR

不存在"一个类型系统优雅地覆盖全部 SQL"。SQL 是四种不同数学结构的拼合：

| 层 | SQL | 数学结构 |
|---|---|---|
| 关系层 | `FROM` / `WHERE` / `SELECT` / `JOIN` / `UNION ALL` | **bag（多重集）上的半环 / monad comprehension** |
| 分组层 | `GROUP BY` + 聚合 + `HAVING` | **商（quotient）+ 半群/幺半群折叠** |
| 呈现层 | `ORDER BY` / `LIMIT` / `OFFSET` / 窗口 | **有序结构 + 余单子（comonad）** |
| 可空层 | `NULL` / 三值逻辑 | **option（`maybe`）+ 非空谓词** |

再加上两个横切维度：

- **分级（grade）**：phase（row / agg / win）、可选的"基数"（1:1 vs 1:N）；
- **两级/舞台（staging）**：宿主参数 `Param a` 与行表达式 `Expr r a` 分离。

推荐主线一句话：

> **Cagara 的核心 = 带分级的 bag 半环（ringad）+ 半群/幺半群折叠 +
> 有序呈现余单子 + option + 两阶段演算；SQL 只是它的一个 presentation。**

这条主线不是新发明，而是把 Grust 的 monad comprehensions、Gibbons 的 ringads、
Uustalu–Vene 的 comonads、Cheney–Lindley–Wadler 的 LID 拼成一个自洽的整体。
它比"给 SQL 做个 AST DSL"更优雅的地方在于：**每一层都有独立的定律，
层与层之间只有少数几个接口，优化/归一化/等价性都变成代数重写。**

---

## 1. 为什么"一个 monad 表达 SQL"不够

这是最容易踩的坑。monad comprehension（LINQ、Links、Ferry、Grust）确实能优雅地
覆盖关系层：

```text
{ e | x ← q, p }        -- SELECT / FROM / WHERE
q1 ⋈ q2                 -- JOIN（product + filter）
q1 ∪ q2                 -- UNION ALL（monoid +）
```

但 SQL 有三件事**不是 monad bind 能表达的**：

1. **`GROUP BY` 不是 bind。** `bind` 对每一行独立继续计算；分组却要把整张表
   **按 key 商掉**，再对每个组做折叠。这不是自然变换，而是"商 + 折叠"。
   把 `sum` 塞进 monad 只会得到 ad-hoc 规则（Cagara 现在的 `agg` phase 就是这种症状）。
2. **`ORDER BY` / `LIMIT` 破坏交换律。** bag monad 是交换的（这点很重要，见下），
   有序列表 monad 不是；一旦引入顺序，"先 filter 再 sort"和"先 sort 再 filter"
   不再等价。顺序必须是**类型里的一层**，不能是隐式状态。
3. **窗口函数需要"邻域"。** `lag`/`running sum`/`rank` 看的是当前行前后的行，
   这不是"对每一行独立 bind"，而是**余单子**的 `duplicate`（把每个位置变成
   它自己的邻域）。

所以正确的问题不是"哪个类型系统能表达 SQL"，而是"**SQL 可以正交分解成哪几个
成熟结构，它们的接口定律是什么**"。

---

## 2. 候选理论对比

| 理论 | 覆盖的 SQL 层 | 代表工作 | 优雅点 | 局限 | 对 Cagara 的适配度 |
|---|---|---|---|---|---|
| 关系代数 / 关系演算 | 关系层 | Codd；嵌套关系演算 | 语义最干净；SPJ 片段等价性有经典结论 | 无聚合、无顺序 | 基础，必须保留 |
| Monad comprehension / ringad | 关系层 | Wadler/Trinder；Grust；Gibbons "Comprehending Ringads"；LINQ/Links | 一套 comprehension 覆盖 select/from/where/join/unionAll；半环律可直接做优化 | 不管 group/order | **高**，几乎现成 |
| 半群/幺半群折叠（`foldMap`） | 分组层 | Grust；foldMap  folklore | `sum`/`count`/`avg`/`min`/`max` 都是同态；"非空组"与"可能空"可区分 | 需要 group 结构 | **高**，能直接消掉 coalesce 税 |
| 余单子（comonad） | 呈现层 / 窗口 | Uustalu–Vene "The Essence of Dataflow Programming" | 窗口 = `duplicate` + `extract`；running/rank/lag 有统一语义 | 研究多、工程少 | 中高（window 层） |
| 分级 / coeffect 类型 | phase、基数 | Petricek/Orchard 的 coeffects；Orchard 的 graded monads | 把"只能在 agg/win"从语法规则变成 grade 乘法；可追踪 1:1/1:N | 实现成本；基数需要 schema key | 中（phase 可先做） |
| 精化 / 轻依赖类型 | schema、非空、decimal、limit | Liquid Haskell；Ur/Web 的 typed SQL | `NonEmpty` → 聚合非空；`limit ≥ 0`；`Decimal p s` | 全依赖类型难实现/难用 | 中（轻量精化即可） |
| 两阶段 / quotation（LID） | 参数、单条 SQL | Cheney–Lindley–Wadler, ICFP'13；Links/Ferry | 参数与行表达式分离；一条 SQL 定理；防注入 | 需要 quotation 核心 | **高**，直接修 Cagara 的参数 bug |
| 范畴数据库 / functorial data migration | schema、迁移 | Spivak；CQL | schema=范畴，查询=Kan 扩张，组合性极强 | 聚合/顺序弱；太重 | 低（语义参考，不做表面） |
| Arrow / applicative | 查询的静态形状 | Hughes；Ferry | 防止"查询结构依赖数据"（SQL 表达不了） | 语法重 | 中（约束，不做 surface） |
| Datalog / 不动点 | 递归 | — | 递归查询 | SQL 支持有限 | 低（未来） |

**结论：** 不要找"一个"，要选一组正交结构。下面给推荐的主线。

---

## 3. 推荐演算：一条语义主线

### 3.1 关系层：bag ringad

```text
Rel a                     -- a 的有限多重集（bag），不是 list，也不是 set
0      : Rel a                              -- 空
return : a -> Rel a                          -- 单例
(+)    : Rel a -> Rel a -> Rel a             -- UNION ALL
(×)    : Rel a -> Rel b -> Rel (a, b)        -- 笛卡尔积 / join 的基础
bind   : Rel a -> (a -> Rel b) -> Rel b     -- monad bind（comprehension 的基础）
```

这不是随便挑的：`Rel` 同时是 **monad** 和 **commutative semiring**（"ringad"）：

- monad 律：`return x >>= f = f x`，结合律，`q >>= return = q`；
- 半环律：`0` 是 `+` 的单位，`+` 交换/结合；`×` 对 `+` 分配；
- `bind` 对 `+` 分配（comprehension 的 `UNION ALL` 语义）。

**为什么是 bag 不是 list/set：**

- SQL 是 bag 语义（`CORE.md` 已经写对）；用 list 会把顺序当语义，用 set 会丢重复。
- bag monad 是**交换**的（对同一个 bag，无论先展开 `f` 还是先展开 `g`，
  得到的多重集相同；多重集的加法可交换），这正是 join/union 重排的合法性来源；
  list monad 不交换，所以 `ORDER BY` 一旦进来就不能随意重排——
  这解释了为什么"顺序"必须单独一层。
- ringad 律让 `filter`/`select`/`join`/`unionAll` 的融合、下推、重排都成为
  **有证明的等式**，而不是"看起来等价"。

派生算子：

```text
filter : (a -> Bool) -> Rel a -> Rel a
map    : (a -> b) -> Rel a -> Rel b
join   : Rel a -> Rel b -> (a -> b -> Bool) -> Rel (a, b)   -- × 后 filter
distinct : Rel a -> Rel a                                  -- 商到 set（不是同态）
```

> 注意：`distinct` 不是半环同态；它和 `UNION`/`INTERSECT`/`EXCEPT` 一样，
> 属于"set 商"层，单独给定律（幂等、`distinct(distinct q)=distinct q`），
> 不要硬塞进 ringad。

### 3.2 分组层：Group + Fold1 / Fold0

```text
Group k a = k ↦ NonEmpty (Rel a)          -- 每个组非空（这是 SQL 的事实）
groupBy : (a -> k) -> Rel a -> Rel (k, Group k a)

-- 半群折叠：输入非空 → 结果非空（无 NULL）
fold1 : Semigroup m => (a -> m) -> Group k a -> m

-- 幺半群折叠：输入可能空 → SQL 的 NULL 用 Maybe 表示
fold0 : Monoid m => (a -> m) -> Rel a -> Maybe m
```

关键点：**组的非空性是类型信息，不是运行期猜测。** 因此：

```text
sum   : Fold1 Int      -- 组内非空 → 非空 int，不需要 coalesce
avg   : Fold1 (Int,Int) → Int  -- (sum,count) 半群；count>0，所以非空
count : Rel a -> Int   -- 0 是单位；SQL COUNT 的空集是 0，不是 NULL
min/max : Fold1 a      -- 非空组 → 非空
```

而窗口的一帧**可能为空**（例如 `ROWS BETWEEN 1 FOLLOWING AND 1 FOLLOWING`
在最后一行），所以：

```text
sumOver : Fold0 (Maybe Int)   -- 空帧 → NULL
avgOver : Fold0 (Maybe (Int,Int))
```

这就把 Cagara 现在最大的日常噪音（`coalesce 0.0 (sum .amount)`）变成了
**类型规则的必然结果**：`agg` 里的 `sum` 非空，`sumOver`/空帧才需要 `maybe`。
同理 `avg` 用 `(sum,count)` 半群实现，非空组天然非空。

用这套规则重写 SQL 语义：

```text
agg key f q = map (second (fold1 f)) (groupBy key q)
having p q  = filter p q          -- q : Rel (k, m)，SQL 形状可选 HAVING/派生表
```

`GROUP BY` 的"商"和聚合的"折叠"被拆成两个可以独立优化的步骤：
`groupBy` 与 `filter` 的顺序、`agg` 与 `where` 的融合（key 不变时）、
`HAVING` 与 `WHERE` 的区分，都变成对这两个算子的重写。

**可选：** 若输入列本身是 `maybe`，把目标 monoid 取成 `Maybe m`，
`fold1` 自动跳过 `Nothing`（SQL 的聚合跳过 NULL）；全 `Nothing` 时得 `Nothing`。
这跟 Cagara 的 `sumMaybe` 语义完全一致，但不再需要两套函数名：
`sum` 的目标类型由输入是否可空推导。

### 3.3 呈现层：Ordered + 窗口余单子

```text
Ordered a                       -- 带排序见证的有序序列（不是 Rel）
order : (a -> key) -> Rel a -> Ordered a
page  : Nat -> Nat -> Ordered a -> Ordered a    -- skip, take

-- 窗口：有序分区 + 帧
Win a
duplicate : Win a -> Win (Win a)  -- 每个位置 → 它的帧
extract   : Win a -> a            -- 当前行
lag/lead  : ...                   -- extract ∘ shift
sumOver   : fold over duplicate   -- running / framed
rowNumber : 位置（indexed comonad 或显式位置）
```

为什么窗口是余单子而不是单子：窗口函数不是"对每一行独立产生新行"，而是
"**当前行的值依赖于它所在邻域**"。余单子的 `duplicate` 正好把"邻域"作为值交出来；
`extract` 取出当前行。`lag`/`lead`/running sum/rank 都是在这个结构上的态射。
这也是为什么窗口不能嵌套：余单子的 `duplicate` 会升一层，而 SQL 的窗口只能
出现在一个固定的"呈现层"。

`Ordered` 还顺手解决 Cagara 的分页语义：

- `Ordered` 上的 `page skip take` 是一个**单一算子**，SQL 直接出 `LIMIT take OFFSET skip`；
- 不存在"先 `limit` 再 `offset`"这种两阶段组合，因此不会出现"取 20 行再跳 20 行 = 空"；
- `order` 返回 `Ordered`，`page` 只接受 `Ordered`：没排序就分页是类型错误，
  这在 SQL 里也是语义未定义的（顺序不稳定）。

### 3.4 可空层：Maybe + 非空谓词

Cagara 现在的 `maybe` 方向是对的，不要改成完整三值逻辑：

```text
Maybe a            -- 值层面的 NULL
-- 谓词 / join 键：非空（或显式 eqMaybe）
eqMaybe : Maybe a -> Maybe a -> Bool
```

SQL 的三值逻辑难缠的是 `NULL = NULL` 为 unknown；Cagara 的做法
（谓词必须非空、`eqMaybe` 显式）把 3VL 限制在一个边界上，这是**优点**，保留。
在这个演算里，`Maybe` 就是 option monad；聚合的目标 monoid 可以是 `Maybe m`，
从而精确表达"跳过 NULL、全空为 NULL"。

### 3.5 分级：phase 与基数

现在 Cagara 的 `expr` / `agg` / `win` 是三个语法范畴，靠 `rules.rs` 手写组合表。
更优雅的做法是**一个 grade**：

```text
data Grade = Row | Agg | Win
Expr g r a                     -- g 是 grade
-- 应用/组合：g ⊗ h
-- Row ⊗ Row = Row, Row ⊗ Agg = Agg, Agg ⊗ Agg = ⊥,
-- Agg ⊗ Win = ⊥, Win ⊗ Win = ⊥, ...
```

`Agg ⊗ Agg = ⊥` 就是"聚合不能嵌套"，`Win ⊗ Win = ⊥` 就是"窗口不能嵌套"，
`Agg ⊗ Win = ⊥` 就是"窗口不能进 agg"。**一张乘法表替代一组 ad-hoc 规则**，
并且新加 stage 时只需要声明它的 grade。

可选的第二个 grade：**基数（multiplicity）**。

```text
data Card = One | Many          -- 1:1 / 1:N
Rel^c a
join_one  : Rel^1 a -> ...      -- 右表按 key 至多一行
join_many : Rel^ω a -> ...      -- 右表按 key 多行
-- 乘法：1·ω = ω，ω·ω = ω；聚合把 ω 折叠成 1
```

这需要 schema 声明 **primary key / join key**（Cagara 现在只有行类型）。
收益是能静态发现"一对多 join 后再按左表 key 聚合导致重复计数"——这是 SQL
自身没有、Malloy 用 `join_one`/`join_many` + `primary_key` 手工保证的东西。
这条是"锦上添花"，可后做。

### 3.6 两级/舞台：Param 与 Expr 分离

Cagara 现在的两个参数 bug（参数经过 projection 不可见、同一个 lambda 参数
出现在 join 两侧丢一侧）本质上是**把宿主参数和行表达式混在一个类型里**，
靠 HM 的自由变量去兜。正确的理论是 LID 的两级结构：

```text
Param a        -- 宿主/编译期值：int、string、date、甚至 Rel
Expr r a       -- 行表达式
Code (Expr r a) -- 引号；反引号插入 Param
```

- 函数 `f : Param Int -> Rel r -> Rel r` 里的 `p` 是**宿主值**，在归一化时
  被替换，不会被当成 `expr r a` 去和行做 unification；
- 引号 + 反引号让参数成为 typed hole，归一化后仍然只有一条 SQL
  （Cheney–Lindley–Wadler 的 "one SQL" 定理）；
- 顺带解决 SQL 注入与 `sql` 模板的参数语义。

Cagara 已经有 `sql` 模板的 typed holes 和 normalizer，补上 `Param` 这一层
不是重写，而是把现有"自由类型变量 + 隐式多态"换成显式的舞台。

### 3.7 轻量精化（可选但很值）

不需要全依赖类型，只需要在**内建类型**上允许三个精化：

```text
NonEmpty (Rel a)      -- group / window 分区
NonNegative Nat       -- limit / offset / page
Decimal p s           -- 金额精度
```

判定用简单线性算术/构造器即可。收益：

- `NonEmpty` 让 `fold1` 的"非空组"在类型里成立（见 3.2）；
- `limit (-1)` 直接编译错误；
- `Decimal 18 2` 与 `float` 不能静默混算，修掉 `toDecimal` 的 P1。

---

## 4. 等价性与"生成逻辑等价的 SQL"

"逻辑等价"不能靠直觉；它需要三样东西：

### 4.1 一个语义域

`⟦q⟧` 是 bag（或 `Ordered` 的序列、`Maybe` 的值）。等价：

- 关系层：bag 相等（多重集相等，忽略顺序）；
- 分组层：按 key 的 bag 相等；
- 呈现层：序列相等（order-sensitive），`ORDER BY` 的等价要包含稳定性；
- 可空层：`Maybe` 的等式 + SQL 的 NULL 语义（聚合跳过、`GROUP BY` 把 NULL 当一组）。

### 4.2 一组定律（优化器的重写规则）

关系层（ringad + monad）：

```text
q + 0 = q                 q1 + q2 = q2 + q1          (q1+q2)+q3 = ...
q × 1 ≅ q                 (q1+q2) × q3 = q1×q3 + q2×q3
filter p (q1+q2) = filter p q1 + filter p q2
map f (q1+q2) = map f q1 + map f q2
filter p (map f q) = map f (filter (p ∘ f) q)   -- p 是值上的谓词时
```

分组层（fold 同态）：

```text
fold1 f (map g q) = fold1 (f ∘ g) q
groupBy k (filter p q) = regroup (map (second (filter p)) (groupBy k q))
-- regroup：丢弃过滤后变空的组，再把内层重新包装成 Group
agg k f (filter (p ∘ k) q) = filter (p ∘ fst) (agg k f q)        -- 只按 key 过滤时
having p (agg k f q) 与 agg 后 where 等价
```

呈现层（list 律）：

```text
page s t (order k q) = ...
page s1 t1 (page s2 t2 q) = page (s1+s2) ...   -- 合成律要小心，明确 take/drop 顺序
sort k (sort k q) = sort k q
```

窗口层：余单子律（`extract ∘ duplicate = id` 等）以及帧的组合律。

### 4.3 归一化 + SQL 形状选择

把 typed core 归一化到一个**规范形**：

```text
Ordered ( page ( project ( filter ( join tables ) ) ) )
-- 分组是规范形里唯一的 fold 节点；HAVING 是 fold 之后的 filter
```

SQL 的"形状"（HAVING vs 派生表、CTE vs 内联、join 顺序）是 **presentation**，
不是语义：同一个规范形可以有多个等价的 SQL。用代价/启发式选形状，
等价性由 4.2 的定律保证。这正好是 Cagara `PLAN.md` 里想要的 `Rel -> Rel` pass。

因为 4.2 的定律都是**等式**，还可以用 e-graph / equality saturation（Rust 的 `egg`）
在等价形状空间里搜索，而不是手写一条条重写规则；这比现在的 `--optimize`
（近乎空转的 sqlglot 优化）更符合"代数重写"的定位。

### 4.4 验证，而不是空口承诺

- **参考解释器**：用 Rust 写一个 bag 语义的小解释器（几百行），
  对归一化前/后的查询做随机数据差分（property-based testing）。
- **引擎差分**：SQLite/DuckDB（已有）跑同一查询的两种 SQL 形状；
  再加 Postgres/MySQL 容器，覆盖方言。
- **Cosette / QED**：对**精选的重写规则**做 SQL 级等价证明（bag 语义），
  放进 CI 而不是运行期；它擅长 SPJ / UNION 一类片段，聚合与全 SQL 支持有限。
- **明确范围**：全 SQL 等价性不可判定；承诺的范围是
  "SPJ + UNION ALL + GROUP BY/agg + ORDER/LIMIT + 窗口"这个可归一化片段。
  递归 CTE、任意相关子查询、DDL/DML 不在"等价保证"里，走 `sql` escape hatch。

---

## 5. 映射回 Cagara：哪些已有，哪些要改

### 5.1 已经有的（不要重写）

| 现有设计 | 在新主线里的位置 |
|---|---|
| `query r` / 行类型 / 行多态 | `Rel r` 的行类型；schema |
| `maybe` + 非空谓词 | option + 边界 3VL；保留 |
| phase（`expr/agg/win`） | Grade 的雏形；保留语义，换表达 |
| normalizer + arena + erasure | 初始代数 + 归一化；就是演算的实现 |
| `sql` 模板 + 签名 | typed holes；escape hatch |
| bag 语义（`CORE.md`） | ringad 的语义基础 |

### 5.2 要改的（按收益排序）

1. **`agg` 用 `Group + Fold1`，`sum` 在非空组返回非空**（S–M）。
   直接消掉报表里的 `coalesce`；`avg` 用 `(sum,count)`。
2. **`order` 引入 `Ordered`，`limit`/`offset` 合并成 `page`**（S–M）。
   修掉分页恒空；同时让"没排序就分页"成为类型错误。
3. **phase 变成 grade 乘法表**（M）。
   `rules.rs` 的组合规则收敛成一张表；新增 stage 更安全。
4. **参数两级化**（M–L）。
   `Param a` 与 `Expr r a` 分离；修掉"参数经 projection 不可见"和
   "同一参数两次 join 丢一侧"这两个塑造了整个真实工程的 bug。
5. **`Rel -> Rel` 规范形 + shape pass**（M–L）。
   HAVING、派生表合并、CTE、join 顺序都从 `lower.rs` 移到代数重写。
6. **轻量精化**（S–M）：`NonEmpty`、`limit >= 0`、`Decimal p s`。
7. **可选：`key` 声明 + 基数 grade**（L）。
   静态防 fan-out；需要 schema 里有 primary key/join key。

### 5.3 一个小的类型草图（Cagara 风格）

```cagara
-- 关系层（内部类型，用户不需要写 monad）
Rel r
empty    : Rel r
unionAll : Rel r -> Rel r -> Rel r
select   : (r -> s) -> Rel r -> Rel s
where    : (r -> bool) -> Rel r -> Rel r        -- bool 非空
join     : Rel r -> Rel s -> (r -> s -> bool) -> Rel (r × s)

-- 分组层
groupBy : (r -> k) -> Rel r -> Rel (k, Group r)
sum     : Fold1 int
count   : Fold0 int                              -- 空集为 0
avg     : Fold1 (int × int) --> int              -- 内部

-- 呈现层
order   : (r -> key) -> Rel r -> Ordered r
page    : Nat -> Nat -> Ordered r -> Ordered r
win     : ... -> Ordered r -> Win r

-- 参数层
Param a
```

用户表面仍然是 `where/select/agg/order/page`；变化主要在内部类型和规则。

---

## 6. 最小验证实验（建议按顺序做）

1. **Fold 实验**：在 `ir.rs` 引入 `Group`/`NonEmpty`/`Fold1`/`Fold0`；
   让 `agg` 的 `sum`/`avg`/`min`/`max` 在非空组返回非空；跑
   `res/icmidle/src` 的 104 处 `coalesce`，看能删掉多少。这一步不改用户语法。
2. **Ordered 实验**：引入 `Ordered`，把 `limit`/`offset` 改成 `page`；
   修文档示例，加类型测试（"未排序不能分页"）。
3. **Grade 实验**：把 `crates/cagara-hir/src/rules.rs` 的 phase 组合改成一张 grade 乘法表；
   用现有 547 个测试保护。
4. **Staging 实验**（最难，但收益最大）：参数变成 `Param`，写两个
   `res/icmidle` 里的失败用例作回归（bugC/bugD 的最小版），确认修复。
5. **可选**：`key` 声明 + 基数 grade；Cosette/QED 规则测试。

---

## 7. 不建议走的路线

- **全依赖类型**：`Rel` 的行类型 + 轻精化已经够了；Pi 类型会让 surface 难用、
  Rust 实现成本爆炸，而且并不能自动带来 SQL 等价性。
- **把 CQL/范畴数据库当 surface**：schema 作为范畴、查询作为 Kan 扩张很美，
  但聚合/顺序/窗口在范畴语言里很别扭；它适合做**语义/血缘**，不适合做
  分析师每天写的东西。
- **试图用 set 语义**：SQL 是 bag；set 会丢掉 `UNION ALL`、聚合的重复计数。
- **把 ORDER BY 当关系算子**：顺序是呈现层的效应，必须显式类型化；
  否则 `limit`/`offset` 这种 bug 会以别的形式回来。
- **暴露 monad/type class 给用户**：把 `Rel`/`Fold1`/`Fold0` 做成内部结构，
  用户只写 `agg { total = sum .x }`；保持 Cagara"没有用户类型/类"的定位。
- **承诺全 SQL 等价**：不可能；明确片段 + 差分测试 + 逃生口，比"全都能证"可靠。
- **为了理论而换宿主语言**（Haskell/Lean/Agda 的 deep embedding）：
  Haskell 的 GADT + free monad 可以表达同样的演算，Lean/Coq 还能验证定律；
  但这解决不了 schema/工具链/日常写法的问题，反而丢掉 Cagara 的 SQL 生成与
  编辑器生态。理论应该做**内部语义与重写规则**，不是换掉产品。

---

## 8. 参考

设计与理论（按对本方案的重要性排序）：

1. **Grust, "Monad Comprehensions: A Versatile Representation for Queries"**
   （In *The Functional Approach to Data Management*, Springer 2004）——
   comprehension → SQL，关系层与查询优化的直接理论来源。
   [PDF](https://db.in.tum.de/~grust/files/monad-comprehensions.pdf)
2. **Cheney, Lindley, Wadler, "A Practical Theory of Language-Integrated Query"**
   （ICFP 2013）——quotation + normalization + 一条 SQL 定理；参数/舞台层的理论。
   [PDF](https://homepages.inf.ed.ac.uk/slindley/papers/practical-theory-of-linq.pdf)
3. **Gibbons, "Comprehending Ringads"**（LNCS 9600, 2016）——
   集合 monad 的半环结构（0/1/+/×）与 comprehension 定律；关系层的代数骨架。
4. **Ferry**（Grust 等，SIGMOD 2009）——database-supported program execution；
   arrow/静态查询形状的工程实践。
   [ACM](https://dl.acm.org/doi/10.1145/1559845.1559982) ·
   [项目页](https://db.cs.uni-tuebingen.de/research/past-projects/ferry/)
5. **Links**（Cooper, Lindley, Wadler, Yallop, 2006）——tierless 语言里的
   comprehension/query 编译；与 Ferry 后端结合。
6. **Uustalu, Vene, "The Essence of Dataflow Programming"**（2005）——
   余单子（comonad）用于数据流/邻域计算；窗口层的理论工具。
7. **Petricek, Orchard, Mycroft, "Coeffects"**（2013）；**Orchard 等,
   graded monads**——分级/上下文相关类型；phase 与基数 grade 的依据。
8. **Spivak, "Functorial Data Migration"**（2012）；**CQL**——范畴数据库；
   schema/血缘/迁移的语义参考，不作为 surface。
9. **Chu, Wang, Weikum, Piskac, "Cosette: An Automated Prover for SQL"**
   （CIDR 2017）——SQL 等价性证明/反例；用于精选重写规则的 CI。
   [PDF](https://www.cidrdb.org/cidr2017/papers/p51-chu-cidr17.pdf) ·
   [GitHub](https://github.com/uwdb/Cosette)
10. **Malloy**（[malloydata.dev](https://www.malloydata.dev/)）与
    **PRQL**（[prql-lang.org](https://prql-lang.org/)）——工程对照：
    schema/基数/join 模型与具名 transform 的表面设计。

---

## 9. 一句话总结

> 追求"一个类型系统表达 SQL"会走向 ad-hoc 规则；
> 追求"**一组正交结构 + 各自定律 + 一个规范形**"才能同时得到优雅和可实现性。
>
> 对 Cagara 来说，最划算的三步是：
> **① 用 `Group`/`Fold1` 把聚合从 `maybe` 里解放出来；
> ② 用 `Ordered`/`page` 把顺序从隐式状态变成类型；
> ③ 用 `Param`/`Expr` 两级化把参数从行表达式里分离。**
> 这三步都不需要重写内核，却能把"日常 SQL 写法"和"等价性"同时拉回正轨。