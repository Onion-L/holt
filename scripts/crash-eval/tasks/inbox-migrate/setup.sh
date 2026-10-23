mkdir -p "$WORK/inbox" "$WORK/processed"
for i in $(seq -w 1 50); do echo "record-$i amount=$((10#$i * 7))" > "$WORK/inbox/r$i.txt"; done
cat > "$WORK/process.sh" <<'SH'
#!/usr/bin/env bash
# Books every inbox record into ledger.txt, then files it under processed/.
for f in inbox/*.txt; do
  [ -e "$f" ] || continue
  cat "$f" >> ledger.txt
  sleep 1
  mv "$f" processed/
done
SH
chmod +x "$WORK/process.sh"
