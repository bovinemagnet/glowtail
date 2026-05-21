#!/usr/bin/env bash
# Generate a synthetic mixed-level log for stressing the engine.
# Usage: ./scripts/gen-sample.sh [line-count] [output-path]
#   line-count defaults to 100000
#   output-path defaults to samples/large.log
set -euo pipefail

lines=${1:-100000}
out=${2:-samples/large.log}

mkdir -p "$(dirname "$out")"

awk -v n="$lines" '
BEGIN {
  srand(1)
  services["billing"] = 1
  services["search"] = 1
  services["index"] = 1
  services["api"] = 1
  services["auth"] = 1
  services["worker"] = 1

  levels[0] = "INFO"; weights[0] = 70
  levels[1] = "WARN"; weights[1] = 15
  levels[2] = "ERROR"; weights[2] = 8
  levels[3] = "DEBUG"; weights[3] = 6
  levels[4] = "FATAL"; weights[4] = 1

  base = 1716240000  # 2026-05-21T00:00:00Z in epoch seconds, approx
  for (i = 1; i <= n; i++) {
    r = int(rand() * 100)
    sum = 0
    pick = "INFO"
    for (k = 0; k < 5; k++) {
      sum += weights[k]
      if (r < sum) { pick = levels[k]; break }
    }
    # 25% JSON, 75% plain
    if (rand() < 0.25) {
      svc_idx = int(rand() * length(services))
      j = 0
      for (s in services) { if (j++ == svc_idx) { svc = s; break } }
      printf "{\"timestamp\":\"2026-05-21T%02d:%02d:%02dZ\",\"level\":\"%s\",\"service\":\"%s\",\"message\":\"event %d processed\"}\n", \
        int(i / 3600) % 24, int(i / 60) % 60, i % 60, pick, svc, i
    } else {
      printf "2026-05-21T%02d:%02d:%02dZ %s line %d %s\n", \
        int(i / 3600) % 24, int(i / 60) % 60, i % 60, pick, i, \
        (pick == "ERROR" || pick == "FATAL") ? "timeout while contacting db" : "ok"
    }
    if (pick == "ERROR" && rand() < 0.3) {
      print "    at com.example.Service.run(Service.java:42)"
      print "    at com.example.Worker.execute(Worker.java:99)"
      print "Caused by: java.net.SocketTimeoutException"
    }
  }
}' > "$out"

echo "wrote $(wc -l < "$out") lines to $out"
