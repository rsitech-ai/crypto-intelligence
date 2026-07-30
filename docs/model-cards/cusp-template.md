# Cusp Structural Model Card

Schema version: `1`

Gate decision: `{{decision}}`

Production probability use: `{{production_probability_use}}`

This report evaluates whether the frozen stochastic-cusp candidate has stable incremental walk-forward value beyond the required volatility, leverage, regime, and microstructure controls. Eligibility permits a later stacker review; it does not assign a production weight.

## Evidence identity

- Input BLAKE3: `{{input_blake3}}`
- Dataset manifest BLAKE3: `{{dataset_manifest_blake3}}`
- Candidate model BLAKE3: `{{candidate_model_blake3}}`
- Baseline model BLAKE3: `{{baseline_model_blake3}}`
- Validated report BLAKE3: `{{report_evidence_blake3}}`
- Gate evaluation BLAKE3: `{{gate_evidence_blake3}}`

## Required interpretation

The product convention is `V(y; alpha, beta) = y^4/4 - beta*y^2/2 - alpha*y`. Fold distance, barrier height, and cusp-region probability are structural diagnostics, not calibrated crash probabilities.

The gate is predeclared and fail-closed. It requires positive Brier skill over the same ensemble without cusp features, improvement in at least 70% of untouched outer folds, at least 50 positive events, improvement across at least three regimes, multiple independent event episodes, safe recent-fold behavior, calibration bounds, alert-budget improvement, coefficient stability, sign/scaling parity, all required control baselines, and zero numerical failures.

## Limitations

- This artifact does not prove live shadow stability, production runtime behavior, package signing, notarization, or release readiness.
- An eligible decision does not mean the cusp module is currently used in a production probability.
- A research-only decision keeps the structural panel available as experimental evidence and must display “not used in production probability.”
- Economic or trading performance is outside this gate; no execution authority is created.
