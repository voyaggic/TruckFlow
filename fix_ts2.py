import sys

with open('D:/Exhauster project/TruckFlow/src/sections/AnprConfig.tsx', 'r', encoding='utf-8') as f:
    content = f.read()

# Fix the checkbox - replace preferCloud with config?.prefer_cloud
old_checkbox = '''<input
              type="checkbox"
              checked={preferCloud}
              onChange={(e) => setPreferCloud(e.target.checked)}
              disabled={config === null}
              aria-label="Prefer cloud OCR engine for character reading"
            />'''

new_checkbox = '''<input
              type="checkbox"
              checked={config?.prefer_cloud === true}
              onChange={(e) => setConfig(prev => ({ ...prev, prefer_cloud: e.target.checked }))}
              disabled={config === null}
              aria-label="Prefer cloud OCR engine for character reading"
            />'''

if old_checkbox in content:
    content = content.replace(old_checkbox, new_checkbox)
    print("Checkbox fixed OK")
else:
    print("Old checkbox not found")
    idx = content.find('preferCloud')
    if idx >= 0:
        print(f"Found preferCloud at {idx}")

# Remove any remaining setPreferCloud and preferCloud references  
content = content.replace('setPreferCloud', '')
content = content.replace('preferCloud', 'config?.prefer_cloud')

with open('D:/Exhauster project/TruckFlow/src/sections/AnprConfig.tsx', 'w', encoding='utf-8') as f:
    f.write(content)

print("Done writing")