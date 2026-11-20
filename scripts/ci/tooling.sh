#!/bin/bash
# Exercise selection and result contracts before scheduling expensive checks.
set -euo pipefail
python3 -m unittest discover -s scripts -p test_ci.py

python3 -m unittest discover -s scripts -p test_identifier.py
