COPY (
  SELECT i_category, SUM(ss_sales_price * ss_quantity) AS revenue
  FROM store_sales
  JOIN item ON ss_item_sk = i_item_sk
  JOIN date_dim ON ss_sold_date_sk = d_date_sk
  WHERE d_year = 1994
  GROUP BY i_category
  ORDER BY revenue DESC, i_category
) TO 'outputs/result.csv' (HEADER, DELIMITER ',');
