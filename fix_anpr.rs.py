import sys

with open('D:/Exhauster project/TruckFlow/src-tauri/src/anpr.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# Replace the UPDATE query params - add prefer_cloud after is_capture_point
old_params = """            is_capture_point = COALESCE(?9, is_capture_point),
            designated_machine_id = COALESCE(?10, designated_machine_id),"""

new_params = """            is_capture_point = COALESCE(?9, is_capture_point),
            prefer_cloud = COALESCE(?10, prefer_cloud),
            designated_machine_id = COALESCE(?11, designated_machine_id),"""

if old_params in content:
    content = content.replace(old_params, new_params)
    print("Replaced query lines")
else:
    print("ERROR: Could not find old query lines")
    # Let's debug - find where is_capture_point appears
    idx = content.find("is_capture_point = COALESCE")
    if idx >= 0:
        print(f"Found at index {idx}: {content[idx:idx+200]}")
    else:
        print("is_capture_point not found at all")

# Also replace the params array - add prefer_cloud after is_capture_point
old_params2 = """            is_capture_point.map(|b| if b { 1 } else { 0 }),
            designated_machine_id,"""

new_params2 = """            is_capture_point.map(|b| if b { 1 } else { 0 }),
            prefer_cloud,
            designated_machine_id,"""

if old_params2 in content:
    content = content.replace(old_params2, new_params2)
    print("Replaced params array")
else:
    print("ERROR: Could not find old params lines")
    idx = content.find("is_capture_point.map")
    if idx >= 0:
        print(f"Found at index {idx}: {content[idx:idx+200]}")

with open('D:/Exhauster project/TruckFlow/src-tauri/src/anpr.rs', 'w', encoding='utf-8') as f:
    f.write(content)

print("Done writing file")