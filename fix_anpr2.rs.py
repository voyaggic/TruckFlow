with open('D:/Exhauster project/TruckFlow/src-tauri/src/anpr.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# 1. Add prefer_cloud = COALESCE(?10, prefer_cloud), after is_capture_point line
# Current lines 145-147:
#             is_capture_point = COALESCE(?9, is_capture_point),
#             max_pending_duration_hours = COALESCE(?10, max_pending_duration_hours),
#             designated_machine_id = COALESCE(?11, designated_machine_id),

# New lines 145-149:
#             is_capture_point = COALESCE(?9, is_capture_point),
#             prefer_cloud = COALESCE(?10, prefer_cloud),
#             designated_machine_id = COALESCE(?11, designated_machine_id),
#             updated_by = ?12, updated_at = ?13

old_block1 = """            is_capture_point = COALESCE(?9, is_capture_point),
            max_pending_duration_hours = COALESCE(?10, max_pending_duration_hours),
            designated_machine_id = COALESCE(?11, designated_machine_id),
            updated_by = ?12, updated_at = ?13"""

new_block1 = """            is_capture_point = COALESCE(?9, is_capture_point),
            prefer_cloud = COALESCE(?10, prefer_cloud),
            designated_machine_id = COALESCE(?11, designated_machine_id),
            updated_by = ?12, updated_at = ?13"""

if old_block1 in content:
    content = content.replace(old_block1, new_block1)
    print("Block 1 replaced OK")
else:
    print("Block 1 NOT found - searching...")
    idx = content.find("is_capture_point = COALESCE(?9, is_capture_point)")
    if idx >= 0:
        print(f"Found at {idx}: {content[idx:idx+150]}")

# 2. Update params array - add prefer_cloud after is_capture_point.map
old_block2 = """            is_capture_point.map(|b| if b { 1 } else { 0 }),
            max_pending_duration_hours,"""

new_block2 = """            is_capture_point.map(|b| if b { 1 } else { 0 }),
            prefer_cloud,
            max_pending_duration_hours,"""

if old_block2 in content:
    content = content.replace(old_block2, new_block2)
    print("Block 2 replaced OK")
else:
    print("Block 2 NOT found")

# 3. Update the params array end to include prefer_cloud before designated_machine_id
old_block3 = """            is_capture_point.map(|b| if b { 1 } else { 0 }),
            max_pending_duration_hours,
            designated_machine_id,"""

new_block3 = """            is_capture_point.map(|b| if b { 1 } else { 0 }),
            prefer_cloud,
            max_pending_duration_hours,
            designated_machine_id,"""

if old_block3 in content:
    content = content.replace(old_block3, new_block3)
    print("Block 3 replaced OK")
else:
    print("Block 3 NOT found")

with open('D:/Exhauster project/TruckFlow/src-tauri/src/anpr.rs', 'w', encoding='utf-8') as f:
    f.write(content)

print("Done")