"""Format one oha JSON result as a table row: bench_fmt.py <json> <label> <idle_rss_kb> <loaded_rss_kb>"""
import json, sys

d = json.loads(sys.argv[1])
s, l = d["summary"], d["latencyPercentiles"]
print(f"{sys.argv[2]:<22} {s['requestsPerSec']:8.0f} req/s   p50 {l['p50']*1000:6.2f} ms   p99 {l['p99']*1000:6.2f} ms   "
      f"RSS idle {int(sys.argv[3])//1024:4d} MB, loaded {int(sys.argv[4])//1024:4d} MB   status {d['statusCodeDistribution']}")
