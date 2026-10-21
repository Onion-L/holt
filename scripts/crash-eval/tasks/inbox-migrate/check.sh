[ -z "$(ls inbox)" ] || { echo "inbox not empty"; exit 1; }
[ "$(ls processed | wc -l | tr -d ' ')" = 50 ] || { echo "processed count"; exit 1; }
diff <(cat processed/*.txt | sort) <(sort ledger.txt)
