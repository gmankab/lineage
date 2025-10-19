set -ueo pipefail
set +x

export start=$(date +%s)
export end=$(($start + 360))
while true; do
  export now=$(date +%s)
  [ $now -gt $end ] && break
  [ -e $dir/started ] && break
  sleep 10
done

if [ ! -e "$dir/started" ]; then
  echo 'error: build not started'
  exit 1
fi

export start=$(date +%s)
export end=$(($start + 21600))
while true; do
  export now=$(date +%s)
  [ $now -gt $end ] && break
  [ -e $dir/done ] && break
  sleep 10
done
