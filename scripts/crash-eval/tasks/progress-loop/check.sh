diff <(seq -f 'line %g' 1 60) progress.txt && [ "$(cat done.txt)" = DONE ]
