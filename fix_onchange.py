with open('D:/Exhauster project/TruckFlow/src/sections/AnprConfig.tsx', 'r', encoding='utf-8') as f:
    content = f.read()

old = """onChange={(e) => setConfig(prev => Object.assign({}, prev, { prefer_cloud: e.target.checked }))}"""

new = """onChange={(e) => {
              const val = e.target.checked;
              setConfig(c => ({ ...c, prefer_cloud: val }));
            }}"""

if old in content:
    content = content.replace(old, new)
    print("onChange fixed OK")
else:
    print("old pattern not found - checking for variations...")
    # Try to find the exact line
    idx = content.find('Object.assign')
    if idx >= 0:
        print(f"Found Object.assign at {idx}")

with open('D:/Exhauster project/TruckFlow/src/sections/AnprConfig.tsx', 'w', encoding='utf-8') as f:
    f.write(content)

print("Done")