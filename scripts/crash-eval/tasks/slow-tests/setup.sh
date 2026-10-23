cd "$WORK"
cat > stats.py <<'PY'
def mean(xs):
    return sum(xs) / len(xs)


def median(xs):
    s = sorted(xs)
    return s[len(s) // 2]


def variance(xs):
    m = mean(xs)
    return sum((x - m) ** 2 for x in xs) / len(xs) - 1
PY
mkdir -p tests
for n in $(seq 1 12); do
cat > tests/test_$n.py <<PY
import time, sys
sys.path.insert(0, ".")
from stats import mean, median, variance
time.sleep(5)
assert mean([1, 2, 3, 4]) == 2.5
assert median([4, 1, 3, 2]) == 2.5
assert median([3, 1, 2]) == 2
assert abs(variance([2, 4, 4, 4, 5, 5, 7, 9]) - 4.0) < 1e-9
print("test $n ok")
PY
done
cat > run_tests.sh <<'SH'
#!/usr/bin/env bash
# Runs every test file in order; about 60 seconds.
set -e
for t in tests/test_*.py; do python3 "$t"; done
echo ALL PASSED
SH
chmod +x run_tests.sh
