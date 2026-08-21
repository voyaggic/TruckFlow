with open('D:/Exhauster project/TruckFlow/src/sections/AnprConfig.tsx', 'r', encoding='utf-8') as f:
    content = f.read()

old = """const [notice, setNotice] = useState<string | null>(null);

  

  const refresh"""

new = """const [notice, setNotice] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<AnprTabId>("live");
  const [cameras, setCameras] = useState<CameraSourceView[]>([]);

  const refresh"""

if old in content:
    content = content.replace(old, new)
    print("Added activeTab state OK")
else:
    print("Pattern not found")

with open('D:/Exhauster project/TruckFlow/src/sections/AnprConfig.tsx', 'w', encoding='utf-8') as f:
    f.write(content)

print("Done")