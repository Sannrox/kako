def transform(inputs):
    customers = inputs["customers"].to_pandas()
    orders = inputs["orders"].to_pandas()
    merged = orders.merge(customers, left_on="customer_id", right_on="id")
    totals = (
        merged.groupby(["id", "name"], as_index=False)["amount"]
        .sum()
        .rename(columns={"id": "customer_id", "amount": "total"})
        .sort_values("customer_id")
    )
    return {"totals.csv": totals}
