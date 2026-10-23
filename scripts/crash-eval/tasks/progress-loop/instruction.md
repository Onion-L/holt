Run this bash command and wait for it to finish:

for i in $(seq 1 60); do echo "line $i" >> progress.txt; sleep 1; done

When you are done, progress.txt must hold exactly 60 lines, "line 1" through "line 60", each once and in order. Then create done.txt containing DONE.
