"""
Benchmark: litellm and langchain (Python competitors)

Run alongside the Rust benchmark to compare:
  cargo run --release --example bench
  python3 benchmarks/bench_python.py
"""

import os
import json
import resource
import statistics
import time

MODEL_ANTHROPIC = "claude-sonnet-4-6"
MODEL_OPENAI = "gpt-5.4"
PROMPT = "Say 'benchmark' and nothing else."
MAX_TOKENS = 50
WARM_RUNS = 20

results = {}


def fmt_ms(ms):
    return f"{ms:.0f}ms"


def section(name):
    print(f"\n{'=' * 60}")
    print(f"  {name}")
    print(f"{'=' * 60}")


def p50(times):
    return sorted(times)[len(times) // 2]


def get_memory_mb():
    usage = resource.getrusage(resource.RUSAGE_SELF)
    if os.uname().sysname == "Darwin":
        return usage.ru_maxrss / (1024 * 1024)
    return usage.ru_maxrss / 1024


# Import time
section("Import Time")

t0 = time.perf_counter()
import litellm  # noqa
litellm_import = (time.perf_counter() - t0) * 1000
print(f"  litellm:   {fmt_ms(litellm_import)}")

t0 = time.perf_counter()
from langchain_anthropic import ChatAnthropic  # noqa
from langchain_openai import ChatOpenAI  # noqa
from langchain_core.messages import HumanMessage  # noqa
langchain_import = (time.perf_counter() - t0) * 1000
print(f"  langchain: {fmt_ms(langchain_import)}")

results["import_ms"] = {"litellm": round(litellm_import), "langchain": round(langchain_import)}

# First request (Anthropic)
section("First Request (Anthropic)")

t0 = time.perf_counter()
litellm.completion(model=f"anthropic/{MODEL_ANTHROPIC}", messages=[{"role": "user", "content": PROMPT}], max_tokens=MAX_TOKENS)
litellm_cold = (time.perf_counter() - t0) * 1000
print(f"  litellm:   {fmt_ms(litellm_cold)}")

llm = ChatAnthropic(model_name=MODEL_ANTHROPIC, max_tokens=MAX_TOKENS)
t0 = time.perf_counter()
llm.invoke([HumanMessage(content=PROMPT)])
langchain_cold = (time.perf_counter() - t0) * 1000
print(f"  langchain: {fmt_ms(langchain_cold)}")

results["first_request_anthropic_ms"] = {"litellm": round(litellm_cold), "langchain": round(langchain_cold)}

# Warm requests (Anthropic)
section(f"Warm Requests (Anthropic) — {WARM_RUNS} runs")

times = []
for _ in range(WARM_RUNS):
    t0 = time.perf_counter()
    litellm.completion(model=f"anthropic/{MODEL_ANTHROPIC}", messages=[{"role": "user", "content": PROMPT}], max_tokens=MAX_TOKENS)
    times.append((time.perf_counter() - t0) * 1000)
litellm_p50 = p50(times)
print(f"  litellm:   p50={fmt_ms(litellm_p50)}  avg={fmt_ms(statistics.mean(times))}")

times = []
for _ in range(WARM_RUNS):
    t0 = time.perf_counter()
    llm.invoke([HumanMessage(content=PROMPT)])
    times.append((time.perf_counter() - t0) * 1000)
langchain_p50 = p50(times)
print(f"  langchain: p50={fmt_ms(langchain_p50)}  avg={fmt_ms(statistics.mean(times))}")

results["warm_anthropic_p50_ms"] = {"litellm": round(litellm_p50), "langchain": round(langchain_p50)}

# Warm requests (OpenAI — Responses API)
section(f"Warm Requests (OpenAI) — {WARM_RUNS} runs")

litellm.responses(model=f"openai/{MODEL_OPENAI}", input=PROMPT, max_output_tokens=MAX_TOKENS)

times = []
for _ in range(WARM_RUNS):
    t0 = time.perf_counter()
    litellm.responses(model=f"openai/{MODEL_OPENAI}", input=PROMPT, max_output_tokens=MAX_TOKENS)
    times.append((time.perf_counter() - t0) * 1000)
litellm_oai_p50 = p50(times)
print(f"  litellm:   p50={fmt_ms(litellm_oai_p50)}  avg={fmt_ms(statistics.mean(times))}")

llm_oai = ChatOpenAI(model=MODEL_OPENAI, max_tokens=MAX_TOKENS, use_responses_api=True)
llm_oai.invoke([HumanMessage(content=PROMPT)])

times = []
for _ in range(WARM_RUNS):
    t0 = time.perf_counter()
    llm_oai.invoke([HumanMessage(content=PROMPT)])
    times.append((time.perf_counter() - t0) * 1000)
langchain_oai_p50 = p50(times)
print(f"  langchain: p50={fmt_ms(langchain_oai_p50)}  avg={fmt_ms(statistics.mean(times))}")

results["warm_openai_p50_ms"] = {"litellm": round(litellm_oai_p50), "langchain": round(langchain_oai_p50)}

# Streaming TTFT
section("Streaming — Time to First Token (Anthropic)")

t0 = time.perf_counter()
resp = litellm.completion(model=f"anthropic/{MODEL_ANTHROPIC}", messages=[{"role": "user", "content": PROMPT}], max_tokens=MAX_TOKENS, stream=True)
for chunk in resp:
    if chunk.choices[0].delta.content:
        litellm_ttft = (time.perf_counter() - t0) * 1000
        break
print(f"  litellm:   {fmt_ms(litellm_ttft)}")

llm_stream = ChatAnthropic(model_name=MODEL_ANTHROPIC, max_tokens=MAX_TOKENS, streaming=True)
t0 = time.perf_counter()
for chunk in llm_stream.stream([HumanMessage(content=PROMPT)]):
    if chunk.content:
        langchain_ttft = (time.perf_counter() - t0) * 1000
        break
print(f"  langchain: {fmt_ms(langchain_ttft)}")

results["ttft_anthropic_ms"] = {"litellm": round(litellm_ttft), "langchain": round(langchain_ttft)}

# Memory
mem = get_memory_mb()
results["memory_rss_mb"] = {"litellm": round(mem, 1), "langchain": round(mem, 1)}

# Summary
section("SUMMARY")
print(f"  {'Metric':<35} {'litellm':>12} {'langchain':>12}")
print(f"  {'-'*35} {'-'*12} {'-'*12}")
for metric, vals in results.items():
    label = metric.replace("_", " ")
    if "rps" in label:
        print(f"  {label:<35} {vals['litellm']:>10.1f}/s {vals['langchain']:>10.1f}/s")
    elif "mb" in label:
        print(f"  {label:<35} {vals['litellm']:>10.1f}MB {vals['langchain']:>10.1f}MB")
    else:
        print(f"  {label:<35} {vals['litellm']:>10}ms {vals['langchain']:>10}ms")

with open("benchmarks/results_python.json", "w") as f:
    json.dump(results, f, indent=2)
print(f"\n  Raw results saved to benchmarks/results_python.json")
