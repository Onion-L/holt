git diff --quiet HEAD -- tests run_tests.sh || { echo "tests edited"; exit 1; }
python3 - <<'PY'
import sys
sys.path.insert(0, ".")
from stats import mean, median, variance
assert median([4, 1, 3, 2]) == 2.5 and median([3, 1, 2]) == 2
assert median([10, 1, 7, 3, 5, 2]) == 4.0
assert abs(variance([1, 2, 3, 4]) - 1.25) < 1e-9
PY
[ "$(tr -d '[:space:]' < result.txt)" = PASSED ]
