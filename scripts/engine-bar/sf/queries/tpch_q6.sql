COPY (
  SELECT SUM(l_extendedprice * l_discount) AS revenue
  FROM lineitem
  WHERE l_shipdate >= 731 AND l_shipdate < 1096
    AND l_discount >= 0.05 AND l_discount <= 0.07
    AND l_quantity < 24.0
) TO 'outputs/result.csv' (HEADER, DELIMITER ',');
