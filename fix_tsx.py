with open('D:/Exhauster project/TruckFlow/src/sections/AnprConfig.tsx', 'r', encoding='utf-8') as f:
    content = f.read()

# Replace the entire prefer cloud section
old_section = '''<p className="muted small" style={{ marginTop: 4, marginBottom: 0 }}>
          When enabled, the configured cloud OCR API is tried first for each plate read.
          If the cloud API is unreachable or returns no result, the local OCR engine
          (PaddleOCR or EasyOCR) is used automatically — the read never fails.
        </p>
      </div>

      <div className="field">
        <label>Prefer cloud OCR</label>
        <div className="switch">
          <input
            type="checkbox"
            checked={preferCloud}
            onChange={(e) => setPreferCloud(e.target.checked)}
            disabled={config === null}
            aria-label="Prefer cloud OCR engine for character reading"
          />
          <span className="slider round"></span>
        </div>'''

new_section = '''<p className="muted small" style={{ marginTop: 4, marginBottom: 0 }}>
          When enabled, the configured cloud OCR API is tried first for each plate read.
          If the cloud API is unreachable or returns no result, the local OCR engine
          (PaddleOCR or EasyOCR) is used automatically — the read never fails.
        </p>
      </div>

      <div className="field">
        <label>Prefer cloud OCR</label>
        <div className="switch">
          <input
            type="checkbox"
            checked={config?.prefer_cloud === true}
            onChange={(e) => setConfig(prev => ({ ...prev, prefer_cloud: e.target.checked }))}
            disabled={config === null}
            aria-label="Prefer cloud OCR engine for character reading"
          />
          <span className="slider round"></span>
        </div>'''

if old_section in content:
    content = content.replace(old_section, new_section)
    print("Section replaced OK")
else:
    print("Section NOT found - checking what's there...")
    idx = content.find("checked={preferCloud}")
    if idx >= 0:
        print(f"Found preferCloud at {idx}")

# Also need to remove the preferCloud state declaration
old_state = '''  const [activeTab, setActiveTab] = useState<AnprTabId>("live");
  const [preferCloud, setPreferCloud] = useState<boolean>(false);'''

new_state = '''  const [activeTab, setActiveTab] = useState<AnprTabId>("live");'''

if old_state in content:
    content = content.replace(old_state, new_state)
    print("State removed OK")
else:
    print("State not found")

# Remove prefer_cloud from buildChanges (since we're using config directly)
old_bc = '''    prefer_cloud: preferCloud,'''

new_bc = '''    prefer_cloud: config?.prefer_cloud,'''

if old_bc in content:
    content = content.replace(old_bc, new_bc)
    print("buildChanges prefer_cloud fixed OK")
else:
    print("buildChanges not found - checking...")
    idx = content.find("prefer_cloud:")
    if idx >= 0:
        print(f"Found prefer_cloud at {idx}")

with open('D:/Exhauster project/TruckFlow/src/sections/AnprConfig.tsx', 'w', encoding='utf-8') as f:
    f.write(content)

print("Done")