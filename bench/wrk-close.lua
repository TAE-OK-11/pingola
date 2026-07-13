wrk.headers["Connection"] = "close"

done = function(summary, latency, requests)
  io.write(string.format(
    "LATENCY_US p50=%.0f p90=%.0f p95=%.0f p99=%.0f p999=%.0f max=%.0f\n",
    latency:percentile(50), latency:percentile(90), latency:percentile(95),
    latency:percentile(99), latency:percentile(99.9), latency.max))
end
