import polars as pl


def transform(inputs):
    customers = inputs["customers"]
    orders = inputs["orders"]
    totals = (
        orders.join(customers, left_on="customer_id", right_on="id")
        .group_by(["customer_id", "name"])
        .agg(total=pl.col("amount").sum())
        .sort("customer_id")
    )
    return {"totals.csv": totals}
