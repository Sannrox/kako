COPY (
  SELECT l_orderkey, o_orderdate, o_shippriority,
         SUM(l_extendedprice * (1.0 - l_discount)) AS revenue
  FROM customer
  JOIN orders ON c_custkey = o_custkey
  JOIN lineitem ON l_orderkey = o_orderkey
  WHERE c_mktsegment = 'BUILDING'
    AND o_orderdate < 1169
    AND l_shipdate > 1169
  GROUP BY l_orderkey, o_orderdate, o_shippriority
  ORDER BY revenue DESC, o_orderdate, l_orderkey
  LIMIT 10
) TO 'outputs/result.csv' (HEADER, DELIMITER ',');
