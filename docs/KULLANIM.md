# ⚡ TrackerTop Kullanım Rehberi (Türkçe)

**TrackerTop**, yapay zeka kodlama araçlarının (Claude Code, Cursor vb.) projeyi çok daha az token harcayarak ve çok daha hızlı anlaması için geliştirilmiş bir **kod bilgi grafiği (knowledge graph) ve token tasarruf takipçisidir**.

## Temel Komutlar

### 1. Projeyi İndeksle
Projedeki tüm Rust, HTML, WGSL, PowerShell, Markdown ve TOML dosyalarını tarar ve `.trackertop/index.db` veritabanına kaydeder:
```powershell
.\target\release\trackertop.exe index
```

### 2. Claude Code'a Bağla (Tek Komut)
```powershell
# Sadece bu proje için:
.\target\release\trackertop.exe install claude

# Bilgisayarındaki TÜM projelerde geçerli olsun istersen:
.\target\release\trackertop.exe install claude --global
```
Bu komut Claude Code için MCP bağlantısını, `.claude/skills/trackertop/SKILL.md` yeteneğini ve `CLAUDE.md` kuralını otomatik oluşturur.

### 3. Ne Kadar Token Kurtardığını Gör
```powershell
.\target\release\trackertop.exe stats
```

### 4. İnteraktif Grafik & Özet Rapor Üret
```powershell
.\target\release\trackertop.exe report
```
Oluşan `.trackertop\report.html` dosyasına çift tıklayarak tarayıcıda **interaktif kod grafiğini** ve **token tasarruf grafiğini** inceleyebilirsin. Ayrıca `.trackertop\GRAPH_REPORT.md` dosyasında yazılı mimari özet oluşur.

### 5. Örnek Sorgular (İstersen Elle Test Etmek İçin)
```powershell
.\target\release\trackertop.exe query map
.\target\release\trackertop.exe query outline src/context.rs
.\target\release\trackertop.exe query symbol task_context
.\target\release\trackertop.exe query impact index_repository
.\target\release\trackertop.exe query pack "token savings mcp"
```
