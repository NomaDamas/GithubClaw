# Product E2E Harness Notes

This directory holds maintainer-facing checks for the product E2E harness.

- `test_product_e2e.py` covers helper logic that should stay deterministic.
- Live sandbox validation is driven through `make e2e-product-*` targets and `scripts/product_e2e.py`.
- Heavy live runs are intended for release-stage verification, not per-PR CI.
