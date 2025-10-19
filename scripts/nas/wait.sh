set -ueo pipefail
set +x

export start=$(date +%s)
export end=$(($start + 21600))

while true; do
  export now=$(date +%s)
  [ $now -gt $end ] && break
  [ -e $file ] && break
  sleep 10
done
