# Oasis

支持本地及外部存储、浏览器预览和文件公共链接的自建文件服务器。

[English README](README.md) · [MIT 许可证](LICENSE-MIT) · [第三方致谢及许可声明](THIRD_PARTY_NOTICES.md)

![Oasis 文件浏览器](doc/demo.png)

## 功能

- 用户账户、管理员设置、可选访客访问及文件可见性规则。
- 上传、下载、搜索、目录、ZIP 下载；复制和移动时显示实时进度、阶段及文件名。
- 图片、文本、PDF、音频、视频、字幕及媒体播放列表；实际支持的媒体格式取决于浏览器。
- 界面生成的文件公共链接有效期为六小时。
- 外部存储支持 S3 兼容服务、原生 Backblaze B2、SFTP、FTPS、FTP（不安全）、WebDAV 和 SMB。
- 可选远程文件内容及名称加密；可将外部存储用作主存储，或在本地根目录显示命名的外部文件夹。
- 数据库导出和恢复均需管理员密码验证；备份加密并使用十二个单词的恢复短语。文件内容须单独迁移。
- 英语及中文界面。

## 使用 Docker 运行此版本

在仓库根目录运行：

```sh
docker compose -f compose.example.yaml up --build -d
```

打开 <http://localhost:8000>，初始设置时选择 `/home/storage`。
示例从当前源码构建 `oasis:external-storage-providers`，以 UID/GID **10001**
运行，并使用命名卷保存数据库和本地文件。默认仅绑定宿主机回环地址。
可将 `.env.example` 复制为 `.env` 以修改宿主机绑定地址或端口；使用加密密钥或
证书环境设置前，还须取消 Compose 中相应条目的注释。

现有安装可以保留数据库和文件目录的绑定挂载。替换镜像前应备份这两个目录，
保留已配置的存储路径，并确保 UID/GID 10001 对两个目录均有读写权限。
部署变更时须重新构建镜像并重建容器；仅构建镜像不会替换现有容器。
源码构建镜像与旧的预构建发行包 Dockerfile 不同，后者未设置非 root 用户。

绑定挂载升级、加密密钥、私有 CA、网络访问和临时磁盘空间说明见
[Docker Compose 指南（英文）](doc/external-storage.md#docker-compose)。
需保留命名卷数据时，不要使用 `docker compose down --volumes`。

## 不使用 Docker 的构建及运行

使用与前端锁文件兼容的 Node.js 和 npm，以及 Rust **1.85+** 和本地编译器/链接器。
源码 Docker 构建使用 Node **22** 和 Rust 1.90。在仓库根目录运行：

```sh
node build.js
```

此命令安装前端依赖、构建前后端，并**替换 `release/`**，在 `release/oasis/`
生成发行包。请勿将运行时数据放在会被替换的目录中。
运行其中的 `oasis`（Windows 为 `oasis.exe`），并保留相邻的 `public/` 目录。
将可执行文件旁的 `oasis.conf.sample` 复制为 `oasis.conf`，即可设置监听地址、
端口或 TLS 证书。默认监听 `0.0.0.0:8000`；通过服务器地址访问并完成初始设置。

数据库位于可执行文件旁的 `data/db/`，可删除的临时文件位于 `temp/`。
运行账户须对这些位置及所选本地文件目录具有写入权限。
外部存储另需 **rclone 1.71+**：可安装到 PATH、放在 Oasis 可执行文件旁，
或通过 `OASIS_RCLONE` 指定其路径。独立发行包不包含 rclone。

本地开发及检查：

```sh
cd frontend
npm ci
npm run check
npm run build
cd ../backend
cargo test
cargo run
```

后端通过 <http://localhost:8000> 提供完整应用。`npm run dev` 单独监视并重建前端，
同时启动静态服务器；需要 API 的操作应通过后端地址测试。部分文件系统测试仅适用于 Linux。

## 存储、备份及播放

在**设置 → 外部存储**中配置提供商。Oasis 直接列出存储中的文件，
因此将文件复制到可访问的本地目录后，无需通过界面重新上传或在数据库中注册。
修改存储设置不会自动迁移已有文件。

远程视频通过 Oasis 流式传输，无需先完整下载到磁盘，公共链接同样适用。
其他远程文件响应使用明文磁盘缓存，**闲置二十四小时后过期**，且无容量上限。
启动时清除缓存及临时文件；即使用外部存储作为主存储，也须预留足够的可写磁盘空间。

Safari 使用原生视频控件，其他浏览器使用 Plyr。播放器支持 SRT/VTT 字幕，
实际效果取决于浏览器支持情况。Oasis 提供原始媒体，不进行编码转换或视频修复。
跳转播放位置需要有效的媒体时间戳和索引；更换播放器无法恢复缺失的画面。
PDF 预览由 PDF.js 提供。视频公共链接也可用于 VLC 等外部播放器；
新的请求（包括跳转）须在链接到期前发起。

- [外部存储、加密、缓存、传输验证及测试（英文）](doc/external-storage.md)
- [数据库备份、恢复短语及安装迁移（英文）](doc/database-backups.md)
- [变更日志（英文）](CHANGELOG.md)

## 上游发行版本

此版本扩展自 [machengim/oasis](https://github.com/machengim/oasis)。
[上游发行包](https://github.com/machengim/oasis/releases)、
[上游构建工作流](https://github.com/machengim/oasis/actions/workflows/build_release.yml)
及已发布的 [machengim/oasis 镜像](https://hub.docker.com/r/machengim/oasis)
均为上游资源，**不能据此认定其包含本分支功能**。请构建当前源码以使用本文介绍的实现。

## 致谢及许可

Oasis 保留 Cheng Ma 的原始 [MIT 许可证](LICENSE-MIT)。感谢
[Svelte](https://svelte.dev/)、[Rocket](https://rocket.rs/)、
[Tailwind CSS](https://tailwindcss.com/)、[PDF.js](https://mozilla.github.io/pdf.js/)、
[PDFObject](https://pdfobject.com/)、[Plyr](https://github.com/sampotts/plyr)、
[js-md5](https://github.com/emn178/js-md5) 和 [rclone](https://rclone.org/) 的作者及贡献者。

存储及备份还使用 RustCrypto 的 AEAD、AES-GCM、AES-SIV、Argon2、HKDF 和密钥清零库，
rust-bip39 恢复短语库，以及 base64、data-encoding、serde_json 和 tempfile。
Mozilla 的 source-map 用于前端构建。
[第三方声明](THIRD_PARTY_NOTICES.md) 列出用途、版本、许可证及适用声明，
也包含新增或更新的间接依赖。构建出的发行包和源码 Docker 镜像均附带 Oasis 许可证及第三方声明。
