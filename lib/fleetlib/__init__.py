"""fleetlib: shared secret masking, session-log reading and model classification.

    fleetlib.redact    redact(text) -> (masked_text, count); python -m fleetlib.redact
    fleetlib.logs      iter_units(): typed units from Claude Code and omp logs
    fleetlib.classify  classify(text, options, engine, k) -> {option: probability}
    fleetlib.registry  engine addresses (fleet/fleet.toml, else built-in defaults)

Python 3.11+, standard library only. See README.md next to this package.
"""

__version__ = "0.1.0"
