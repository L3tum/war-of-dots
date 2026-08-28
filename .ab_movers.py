import re, json, html as h


def extract(path):
    txt = open(path).read()
    m = re.search(r'<h2>Raw JSON</h2>\s*<pre>(.*?)</pre>', txt, re.S)
    return json.loads(h.unescape(m.group(1)))


o = extract('reports/stress_22.html')
n = extract('reports/stress_base_r3.html')
om, nm = o['movers_per_tick'], n['movers_per_tick']
print('len old/new:', len(om), len(nm))
diff = [i for i in range(len(om)) if om[i] != nm[i]]
print('indices where movers differ:', len(diff), diff[:20])
print('old first 30:', om[:30])
print('new first 30:', nm[:30])
oz = sum(1 for v in om if v == 0)
nz = sum(1 for v in nm if v == 0)
print('zero counts:', oz, nz)
# where are the zeros?
oz_i = [i for i, v in enumerate(om) if v == 0]
nz_i = [i for i, v in enumerate(nm) if v == 0]
print('old zero idx (first 30):', oz_i[:30])
print('new zero idx (first 30):', nz_i[:30])
