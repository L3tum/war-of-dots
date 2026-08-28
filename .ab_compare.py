import re, json, html as h

def extract(path):
    txt = open(path).read()
    m = re.search(r'<h2>Raw JSON</h2>\s*<pre>(.*?)</pre>', txt, re.S)
    return json.loads(h.unescape(m.group(1)))

old = extract('reports/stress_22.html')
new = extract('reports/stress_base_r3.html')

def sysrow(s, name):
    for r in s['system_timings']:
        if r['name'] == name:
            return r
    return {}

print("== system avg_ms (old -> new) ==")
for name in ['network', 'movement', 'influence', 'supply', 'combat',
             'tickcontext', 'water', 'encirclement']:
    o, n = sysrow(old, name), sysrow(new, name)
    print(f"{name:14} {o.get('avg_ms', 0):9.3f} -> {n.get('avg_ms', 0):9.3f}")

print("\n== latency ==")
for k in ['avg_ms', 'p50_ms', 'p90_ms', 'p95_ms', 'p99_ms']:
    print(f"{k:10} {old['latency'][k]:9.3f} -> {new['latency'][k]:9.3f}")

print("\n== results ==")
for k in ['avg_tick_wall_ms', 'wall_time_ms', 'avg_system_ms',
          'avg_overhead_ms', 'units_remaining']:
    print(f"{k:18} {old['results'][k]:10.3f} -> {new['results'][k]:10.3f}")

print("\n== movers/combat ==")
print("avg_movers", old['results']['avg_movers_per_tick'],
      '->', new['results']['avg_movers_per_tick'])
print("avg_combats", old['results']['avg_combats_per_tick'],
      '->', new['results']['avg_combats_per_tick'])

print("\n== allocs (network system row) ==")
for s, name in [(old, 'old'), (new, 'new')]:
    for r in s.get('system_allocs', {}).get('systems', []):
        if r['name'] == 'network':
            print(name, "allocs/tick", r['allocs_per_tick'],
                  "bytes/tick", r['bytes_per_tick'])

print("\n== new report network section ==")
print(json.dumps(new.get('network'), indent=1))

print("\n== movers distribution (per-tick series, old) ==")
mp = old['movers_per_tick']
zeros = sum(1 for v in mp if v == 0)
print("ticks:", len(mp), "movers==0 ticks:", zeros,
      "movers>0 ticks:", len(mp) - zeros)

mpn = new['movers_per_tick']
zern = sum(1 for v in mpn if v == 0)
print("new movers==0 ticks:", zern, "movers>0:", len(mpn) - zern)

# microbench sanity (environment check): compare a few stable micros
print("\n== microbench medians (old -> new) ==")
def micro(s, i):
    for b in s.get('microbenchmarks', {}).get('benchmarks', []):
        if b['id'] == i:
            return b['median_ns']
    return None
for i in ['ecs/spawn_single', 'ecs/query_positions_10000',
          'systems/movement_500', 'systems/influence_500']:
    o, n = micro(old, i), micro(new, i)
    if o and n:
        print(f"{i:28} {o:9.1f} -> {n:9.1f} ns")
