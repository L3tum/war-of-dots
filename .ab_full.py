import re, json, html as h


def extract(path):
    txt = open(path).read()
    m = re.search(r'<h2>Raw JSON</h2>\s*<pre>(.*?)</pre>', txt, re.S)
    return json.loads(h.unescape(m.group(1)))


o = extract('reports/stress_22.html')
a = extract('reports/stress_base_r3.html')          # splice ON  (new)
b = extract('reports/stress_base_r3_nosplice.html') # splice OFF (new, in-env control)


def net_allocs(s):
    for r in s.get('system_allocs', {}).get('systems', []):
        if r['name'] == 'network':
            return r['allocs_per_tick'], r['bytes_per_tick']
    return None


def series_avg(s, key):
    v = s[key]
    return sum(v) / len(v)


print("=== deterministic metrics: stress_22 (old) vs base_r3 (splice ON) ===")
for label, s in [('stress_22', o), ('base_r3   ', a)]:
    aa, bb = net_allocs(s)
    print(f"{label}: net allocs/tick={aa:12.1f} net bytes/tick={bb/1048576:8.3f} MB  "
          f"wire/tick={series_avg(s, 'net_wire_bytes_per_tick'):14.1f} B  "
          f"updates/tick={series_avg(s, 'net_unit_updates_per_tick'):10.1f}  "
          f"allocs/tick(all)={series_avg(s, 'allocs_per_tick_series'):12.1f}  "
          f"bytes/tick(all)={series_avg(s, 'bytes_per_tick_series')/1048576:8.3f} MB  "
          f"max_alloc={max(s['max_alloc_per_tick_series'])/1048576:8.3f} MB  "
          f"units_rem={s['results']['units_remaining']:.0f}")

print()
print("=== in-env A/B: nosplice (B) vs splice (A) ===")
for label, s in [('nosplice', b), ('splice  ', a)]:
    sp = s['network']['subphase_ms']
    print(f"{label}: net={sp['net']:8.3f} snap={sp['snapshot']:7.3f} delta={sp['delta']:8.3f} "
          f"enc={sp['encode']:8.3f}  wall={s['results']['avg_tick_wall_ms']:8.3f}  "
          f"sys={s['results']['avg_system_ms']:8.3f}")
    aa, bb = net_allocs(s)
    print(f"{'':9} net allocs/tick={aa:12.1f} net bytes/tick={bb/1048576:8.3f} MB  "
          f"wire/tick={series_avg(s, 'net_wire_bytes_per_tick'):14.1f}  "
          f"updates/tick={series_avg(s, 'net_unit_updates_per_tick'):10.1f}")

print()
print("=== regime split (splice ON) ===")
for k, v in a['network']['split'].items():
    print(f"{k:22} ticks={v['ticks']:5d} net_ms={v['net_ms']:8.3f} "
          f"wire={v['net_wire_bytes']/1024:10.1f} KB upd={v['net_unit_updates']:9.1f} "
          f"allocs={v['allocs']:10.1f} bytes={v['bytes']/1048576:8.3f} MB")
print()
print("=== regime split (nosplice, control) ===")
for k, v in b['network']['split'].items():
    print(f"{k:22} ticks={v['ticks']:5d} net_ms={v['net_ms']:8.3f} "
          f"wire={v['net_wire_bytes']/1024:10.1f} KB upd={v['net_unit_updates']:9.1f} "
          f"allocs={v['allocs']:10.1f} bytes={v['bytes']/1048576:8.3f} MB")

# sim nondeterminism between the two new runs
om, nm = a['movers_per_tick'], b['movers_per_tick']
diff = sum(1 for i in range(len(om)) if om[i] != nm[i])
print()
print("movers series differs (splice vs nosplice):", diff, "ticks of 1500")
# per-tick net_ms at aligned spike ticks (drift probe)
import statistics
oa = a['net_ms_per_tick']; ob = b['net_ms_per_tick']
clean = [i for i in range(1500) if i % 3 != 0]
ra = [oa[i] for i in clean]; rb = [ob[i] for i in clean]
print(f"clean-tick net_ms: splice med={statistics.median(ra):7.3f} nosplice med={statistics.median(rb):7.3f}")
dirty = [i for i in range(1500) if i % 3 == 0]
ra = [oa[i] for i in dirty]; rb = [ob[i] for i in dirty]
print(f"spike-tick net_ms: splice med={statistics.median(ra):7.3f} nosplice med={statistics.median(rb):7.3f}")
