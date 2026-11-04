# Build aa-proxy-obd replay fixtures from OBDb test-case YAML.
# usage: python -I testdata/gen_obdb_vectors.py <OBDb dir> profiles testdata/obdb_vectors.json
# Needs PyYAML. <OBDb dir> holds clones of the OBDb vehicle repositories.
import sys, os, re, json, glob, subprocess, yaml
obdb, profdir, out = sys.argv[1:4]
PSI = 6.894757
# OBDb signal-id suffix -> (profile field, scale to profile units)
MAP = {
  'HVBAT_SOC_DISP': ('battery_level_percentage', 1), 'HVBAT_WH': ('battery_level_wh', 1000),
  'HVBAT_SOH': ('battery_state_of_health', 1), 'HVBAT_SOC': ('battery_level_percentage', 1),
  'BATTERY_DC_VOLTAGE': ('battery_voltage', 1), 'BATTERY_CURRENT': ('battery_current', 1),
  'HVBAT_CURR': ('battery_current', 1), 'HVAC_AAT': ('external_temp_celsius', 1),
  'HVBAT_VDC': ('battery_voltage', 1), 'TEMP_INT': ('interior_temp_celsius', 1),
  'TEMP_EXT': ('external_temp_celsius', 1), 'ODO_KM': ('odometer_km', 1),
  'TP_FL': ('tire_fl_kpa', PSI), 'TP_FR': ('tire_fr_kpa', PSI), 'TP_RL': ('tire_rl_kpa', PSI), 'TP_RR': ('tire_rr_kpa', PSI),
  'MG4_SOC': ('battery_level_percentage', 1), 'MG4_AAT': ('external_temp_celsius', 1),
  'ZOE_ODO': ('odometer_km', 1), 'ZOE_SOC_EVC': ('battery_level_percentage', 1),
  # Hyundai/Kia battery, motor and speed signals (EV6, IONIQ 5, Kona Electric)
  'BATTERY_MAX_T': ('battery_temp_max_celsius', 1), 'BATTERY_MIN_T': ('battery_temp_min_celsius', 1),
  'BATTERY_PACK_B01': ('battery_module_1_temp_celsius', 1), 'BATTERY_PACK_B02': ('battery_module_2_temp_celsius', 1),
  'BATTERY_PACK_B03': ('battery_module_3_temp_celsius', 1), 'BATTERY_PACK_B04': ('battery_module_4_temp_celsius', 1),
  'BATTERY_PACK_B05': ('battery_module_5_temp_celsius', 1), 'BATTERY_INLET_T': ('battery_inlet_temp_celsius', 1),
  'AUXILLARY_BATTERY_VOLTAGE': ('aux_battery_voltage', 1), 'DRIVE_MOTOR_SPEED_1': ('motor_rpm', 1),
  'ISOLATION_RESISTANCE': ('isolation_resistance_kohm', 1),
  'HVBAT_T_MAX': ('battery_temp_max_celsius', 1), 'HVBAT_T_MIN': ('battery_temp_min_celsius', 1),
  'HVBAT_MOD1_T': ('battery_module_1_temp_celsius', 1), 'HVBAT_MOD2_T': ('battery_module_2_temp_celsius', 1),
  'HVBAT_MOD3_T': ('battery_module_3_temp_celsius', 1), 'HVBAT_MOD4_T': ('battery_module_4_temp_celsius', 1),
  'HVBAT_INLET_T': ('battery_inlet_temp_celsius', 1), 'VPWR': ('aux_battery_voltage', 1),
  'RPM_2': ('motor_rpm', 1), 'ISO_R': ('isolation_resistance_kohm', 1), 'VSS_7B3': ('speed_kmh', 1),
  'HVBAT_CHG_P_AVAIL': ('charge_power_available_kw', 1), 'HVBAT_DCHG_P_AVAIL': ('discharge_power_available_kw', 1),  # SAE J1979 mode 01 signal names
  'RPM': ('rpm', 1), 'VSS': ('speed_kmh', 1), 'ECT': ('coolant_temp_c', 1),
  'AAT': ('external_temp_celsius', 1), 'FLI': ('fuel_level_pct', 1),
}
# (vector id prefix, profile, OBDb repository, model years replayed or None for all)
SOURCES = [
    ('niro', 'niro', 'Kia-Niro-EV', None),
    ('ev6', 'ev6', 'Kia-EV6', None),
    ('ioniq5', 'ioniq5', 'Hyundai-IONIQ-5', None),
    ('mg4', 'mg4', 'MG-MG4', None),
    ('zoe', 'zoe', 'Renault-ZOE', None),
    ('kona', 'kona', 'Hyundai-Kona-Electric', None),
    # Golf Mk7 (2013-2021) and the e-Golf built on it answer the same mode 01 PIDs on 7E0/7E8.
    ('golf_mk7', 'golf_mk7', 'Volkswagen-Golf', range(2013, 2022)),
    ('golf_mk7-egolf', 'golf_mk7', 'Volkswagen-e-Golf', range(2014, 2021)),
]

def repo_commit(repo):
    return subprocess.check_output(['git', '-C', os.path.join(obdb, repo), 'rev-parse', '--short', 'HEAD'], text=True).strip()
COMMITS = ', '.join(f'{r} {repo_commit(r)}' for _, _, r, _ in SOURCES)

def field_for(sig):
    for suf in sorted(MAP, key=len, reverse=True):
        if sig == suf or sig.endswith('_' + suf):
            return MAP[suf]
    return None

vectors, skipped = [], {}
for prefix, prof, repo, years in SOURCES:
    p = json.load(open(os.path.join(profdir, prof + '.json')))
    pids = {s['pid'].upper(): s for s in p['sources'] if s['kind'] == 'uds_pid'}
    for path in sorted(glob.glob(os.path.join(obdb, repo, 'tests', 'test_cases', '*', 'commands', '*.yaml'))):
        year = path.split(os.sep)[-3]
        if years is not None and int(year) not in years: continue
        name = os.path.basename(path)[:-5]
        parts = re.split(r'[|\uf07c]', name)[0].split('.')
        if len(parts) == 3: hdr, rax, cmd = parts
        elif len(parts) == 2:
            # No receive address in the name: an 11-bit physical request on
            # 7E0-7E7 is answered from the request id + 8 (ISO 15765-4).
            hdr, cmd = parts
            if not re.fullmatch(r'7E[0-7]', hdr.upper()): continue
            rax = format(int(hdr, 16) + 8, 'X')
        else: continue
        if cmd.upper() not in pids: continue
        src = pids[cmd.upper()]
        if len(parts) == 2 and src['ecu_rx'].upper() != rax: continue
        wanted = {f['name'] for f in src['fields']}
        doc = yaml.safe_load(open(path, encoding='utf-8'))
        for i, tc in enumerate(doc.get('test_cases', [])):
            exp = {}
            for sig, val in (tc.get('expected_values') or {}).items():
                m = field_for(sig)
                if m and m[0] in wanted and val is not None:
                    exp[m[0]] = round(val * m[1], 4)
            if not exp:
                skipped['no mapped expected value'] = skipped.get('no mapped expected value', 0) + 1
                continue
            lines = [l.replace(' ', '').upper() for l in str(tc['response']).splitlines() if l.strip()]
            rx = rax or ''
            # 29-bit headers (DADA/DA) print as 18DAF1DA; 11-bit as 7EC.
            pre = ('18DAF1' + rx) if len(rx) == 2 else rx
            frames = [l[len(pre):] for l in lines if pre and l.startswith(pre)]
            if not frames or len(frames) != len(lines):
                frames = [l[len(pre):] for l in lines if l.startswith(pre)]
                if not frames:
                    skipped['no line from the expected ECU'] = skipped.get('no line from the expected ECU', 0) + 1
                    continue
            # Leading consecutive frames (PCI 2N) are leftovers of an earlier
            # reply captured by OBDb; keep the count, decode the real reply.
            stale = 0
            while frames and frames[0][:1] == '2':
                frames.pop(0); stale += 1
            if not frames: continue
            if len(set(frames)) != len(frames) or (frames[0][:1] == '0' and len(frames) > 1):
                skipped['several messages from one ECU'] = skipped.get('several messages from one ECU', 0) + 1
                continue
            vectors.append({'id': f'{prefix}-{cmd}-{year}-{i+1}', 'profile': prof,
                'profile_file': prof, 'pid': cmd.upper(),
                'request_tx': hdr, 'response_rx': rx, 'frames': frames, 'expected': exp,
                'stale_prefix_frames': stale,
                'source': os.path.relpath(path, obdb).replace(chr(92), '/').replace(chr(0xf07c), '|')})
# At most two vectors per (source, pid, model year): the first, plus the first
# of the rest that has stale leading frames, else the second.
groups = {}
for v in vectors:
    groups.setdefault(v['id'].rsplit('-', 1)[0], []).append(v)
vectors = []
for g in groups.values():
    vectors.append(g[0])
    rest = g[1:]
    if rest:
        vectors.append(next((v for v in rest if v['stale_prefix_frames']), rest[0]))
json.dump({'_about': 'Real recorded CAN responses from OBDb test cases, header stripped, ISO-TP PCI kept. '
           'request_tx/response_rx are the addresses OBDb recorded. expected values are OBDb expected values in '
           'profile units (psi*6.894757 = kPa, kWh*1000 = Wh). Generated by testdata/gen_obdb_vectors.py from the OBDb repositories: ' + COMMITS + '.',
           'vectors': vectors}, open(out, 'w'), indent=None, separators=(',', ':'))
from collections import Counter
print(len(vectors), skipped)
print(Counter((v['profile'], v['pid'], v['request_tx'] + '/' + v['response_rx']) for v in vectors))
