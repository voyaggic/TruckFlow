with open('D:/Exhauster project/TruckFlow/src-tauri/src/anpr.rs', 'r', encoding='utf-8') as f:
    content = f.read()

# The current params array (lines 151-165) has 15 entries mapping to ?1-?15
# But the SQL has ?1-?14. I need to:
# 1. Add prefer_cloud at ?10
# 2. Have designated_machine_id at ?11
# 3. Have actor_id at ?12  
# 4. Have now_iso at ?13
# 5. Have ANPR_CONFIG_ID at ?14
# 6. Remove max_pending_duration_hours from params since it's not in SQL UPDATE

# Current state (lines 160-164):
# 160: designated_machine_id,   → should be ?11
# 161: max_pending_duration_hours, → NOT in SQL, should be removed
# 162: actor_id,                → should be ?12
# 163: now_iso(),               → should be ?13
# 164: ANPR_CONFIG_ID,          → should be ?14

# Target params array (positions within params! macro, ?1 to ?14):
# ?1: engine (line 151)
# ?2: confidence_threshold_paddleocr (line 152)
# ?3: confidence_threshold_easyocr (line 153)
# ?4: plate_vehicle_ratio_threshold (line 154)
# ?5: plate_format_rules (line 155)
# ?6: discharge_confirmation_required (line 156)
# ?7: save_recognition_images (line 157)
# ?8: retrain_candidate_threshold (line 158)
# ?9: is_capture_point.map (line 159)
# ?10: prefer_cloud (NEW - insert here)
# ?11: designated_machine_id
# ?12: actor_id
# ?13: now_iso()
# ?14: ANPR_CONFIG_ID

# I need to rebuild the params array from scratch with the correct order.

# First, let me find and replace the entire params! block
old_params_block = """        params![
            engine,
            confidence_threshold_paddleocr,
            confidence_threshold_easyocr,
            plate_vehicle_ratio_threshold,
            plate_format_rules,
            discharge_confirmation_required.map(|b| if b { 1 } else { 0 }),
            save_recognition_images.map(|b| if b { 1 } else { 0 }),
            retrain_candidate_threshold,
            is_capture_point.map(|b| if b { 1 } else { 0 }),
            designated_machine_id,
            max_pending_duration_hours,
            actor_id,
            now_iso(),
            ANPR_CONFIG_ID,
        ],"""

new_params_block = """        params![
            engine,
            confidence_threshold_paddleocr,
            confidence_threshold_easyocr,
            plate_vehicle_ratio_threshold,
            plate_format_rules,
            discharge_confirmation_required.map(|b| if b { 1 } else { 0 }),
            save_recognition_images.map(|b| if b { 1 } else { 0 }),
            retrain_candidate_threshold,
            is_capture_point.map(|b| if b { 1 } else { 0 }),
            prefer_cloud,
            designated_machine_id,
            actor_id,
            now_iso(),
            ANPR_CONFIG_ID,
        ],"""

if old_params_block in content:
    content = content.replace(old_params_block, new_params_block)
    print("Params block replaced OK")
else:
    print("Params block NOT found - trying partial fix...")
    # Try to find the params block and just insert prefer_cloud
    idx = content.find("is_capture_point.map(|b| if b { 1 } else { 0 }),")
    if idx >= 0:
        print(f"Found is_capture_point.map at {idx}")

with open('D:/Exhauster project/TruckFlow/src-tauri/src/anpr.rs', 'w', encoding='utf-8') as f:
    f.write(content)

print("Done")