import re, json, html as h


def extract(path):
    txt = open(path).read()
    m = re.search(r'<h2>Raw JSON</h2>\s*<pre>(.*?)</pre>', txt, re.S)
    return json.loads(h.unescape(m.group(1)))


o = extract('reports/stress_22.html')
n = extract('reports/stress_base_r3.html')
print('OLD config:', json.dumps(o['config']))
print('NEW config:', json.dumps(n['config']))
print()
print('OLD net_ms_per_tick first 40:', [round(v, 1) for v in o['net_ms_per_tick'][:40]])
print('NEW net_ms_per_tick first 40:', [round(v, 1) for v in n['net_ms_per_tick'][:40]])
