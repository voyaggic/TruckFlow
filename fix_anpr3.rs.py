with open('D:/Exhauster project/TruckFlow/src-tauri/src/anpr.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# Swap max_pending_duration_hours and designated_machine_id in params array
# Current order (lines 160-165):
# 160: prefer_cloud,
# 161: max_pending_duration_hours,
# 162: designated_machine_id,
# Need: 160: prefer_cloud, then 161: designated_machine_id, 162: max_pending_duration_hours

old = """            prefer_cloud,
            max_pending_duration_hours,
            designated_machine_id,"""

new = """            designated_machine_id,
            max_pending_duration_hours,"""

if old in content:
    content = content.replace(old, new)
    print("Swapped params OK")
else:
    print("Pattern not found, searching...")
    idx = content.find("prefer_cloud,")
    if idx >= 0:
        print(f"Found prefer_cloud at {idx}: {content[idx:idx+80]}")

with open('D:/Exhauster project/TruckFlow/src-tauri/src/anpr.rs', 'w', encoding='utf-8') as f:
    f.write(content)

print("Done")