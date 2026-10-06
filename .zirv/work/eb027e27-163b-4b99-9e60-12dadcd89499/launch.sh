#!/bin/zsh
# Usage: launch.sh <round> <arm>...  Arms: A = shipped 4.54.0 defaults, T = A + headless dead-tool diet.
R=/tmp/claude-501/fafo-r2
ROUND=$1; shift
BASE=(HOME=$HOME USER=$USER LOGNAME=$LOGNAME SHELL=/bin/zsh TERM=xterm-256color LANG=en_US.UTF-8 TMPDIR=$TMPDIR PATH=$PATH
      ZIRV_CTX_PACE=false ZIRV_CTX_SUPERVISOR_ENABLED=false ZIRV_CTX_MEMORY_HARVEST=false
      FAFO_EFFORT=unset FAFO_LEVERS=off)
ARGS=(--tasks t13_recurring,t14_bugsweep,t15_reports,t16_tags,t17_schema_migration,t18_ledger_layer,t19_goals_saga,t20_audit_log,t21_search,t22_envelopes --conds zirv-nojev --reps 2 --model sonnet
      --parallel 1 --stagger-s 30 --noninteractive --resume
      --zirv-dir ${ZDIR:-$R/bin} --bench-root $R/bench)
cd $R
: > $R/$ROUND.pids
for arm in "$@"; do
  case $arm in
    A) extra=() ;;
    T) extra=(ZIRV_CTX_HEADLESS_DISALLOWED_TOOLS=ScheduleWakeup,Agent,ListAgents,ShareOnboardingGuide,ReportFindings) ;;
  esac
  nohup env -i $BASE $extra python3 $R/harness/run.py $ARGS --runs-subdir runs-$ROUND-$arm > $R/$ROUND-$arm.log 2>&1 < /dev/null &!
  echo "$arm $!" >> $R/$ROUND.pids
done
nohup $R/usage-guard.sh $R/$ROUND.pids > $R/$ROUND-guard.log 2>&1 < /dev/null &!
cat $R/$ROUND.pids
