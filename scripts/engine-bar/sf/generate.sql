-- Deterministic decision-support tables at generator scale {S}: the same
-- loop as the mizuchi suite generator (customer 8S, orders 16S, lineitem 48S,
-- item 8S, store_sales 24S, date_dim 12). Dates are day numbers from
-- 1992-01-01. Run by the default engine; outputs are Snappy Parquet.
COPY (
  SELECT i + 1 AS c_custkey, i % 5 AS c_nationkey,
         ['AUTOMOBILE', 'BUILDING', 'FURNITURE', 'MACHINERY', 'HOUSEHOLD'][i % 5 + 1] AS c_mktsegment
  FROM range(8 * {S}) t(i)
) TO 'outputs/customer.parquet' (FORMAT PARQUET);
COPY (
  SELECT i + 1 AS o_orderkey, i % (8 * {S}) + 1 AS o_custkey,
         CASE WHEN i % 2 = 0 THEN 1169 - 20 - i % 40 ELSE 1169 + 20 + i % 40 END AS o_orderdate,
         i % 3 AS o_shippriority
  FROM range(16 * {S}) t(i)
) TO 'outputs/orders.parquet' (FORMAT PARQUET);
COPY (
  SELECT i % (16 * {S}) + 1 AS l_orderkey,
         CASE WHEN i % 3 = 0 THEN 10.0 ELSE 30.0 + i % 20 END::DOUBLE AS l_quantity,
         (1000.0 + i * 10.0)::DOUBLE AS l_extendedprice,
         CASE WHEN i % 3 = 0 THEN 0.06 ELSE 0.02 + (i % 8) * 0.01 END::DOUBLE AS l_discount,
         (0.01 * (i % 8 + 1))::DOUBLE AS l_tax,
         ['R', 'A', 'N'][i % 3 + 1] AS l_returnflag,
         ['O', 'F'][i % 2 + 1] AS l_linestatus,
         CASE i % 4 WHEN 0 THEN 731 + i % 300 WHEN 1 THEN 1169 + 10 + i % 50 ELSE 2435 - i % 200 END AS l_shipdate
  FROM range(48 * {S}) t(i)
) TO 'outputs/lineitem.parquet' (FORMAT PARQUET);
COPY (
  SELECT i + 1 AS i_item_sk, ['Books', 'Electronics', 'Home'][i % 3 + 1] AS i_category
  FROM range(8 * {S}) t(i)
) TO 'outputs/item.parquet' (FORMAT PARQUET);
COPY (
  SELECT i + 1 AS d_date_sk, 1992 + i % 6 AS d_year
  FROM range(12) t(i)
) TO 'outputs/date_dim.parquet' (FORMAT PARQUET);
COPY (
  SELECT i % (8 * {S}) + 1 AS ss_item_sk, i % 12 + 1 AS ss_sold_date_sk,
         (5.0 + i % 20)::DOUBLE AS ss_sales_price, (1 + i % 5)::DOUBLE AS ss_quantity
  FROM range(24 * {S}) t(i)
) TO 'outputs/store_sales.parquet' (FORMAT PARQUET);
