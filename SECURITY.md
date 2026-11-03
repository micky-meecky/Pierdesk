# Security policy

Pierdesk moves your clipboard, files and, optionally, an SSH session between computers on your network, so security
reports are taken seriously.

## Reporting a vulnerability

Please **do not open a public issue** for a security problem.

Report it privately on GitHub: open the **Security** tab of this repository and choose **Report a vulnerability**
([direct link](https://github.com/micky-meecky/Pierdesk/security/advisories/new)). Only you and the maintainer can see the report.

A useful report includes:

- the Pierdesk version (Settings → About) and your Windows version;
- what you found and why it matters (what an attacker could do);
- steps to reproduce it, and a proof of concept if you have one;
- whether the attacker needs to be on the same local network, have a paired device, or have local access.

Please do not include real passwords, keys, or other people's data.

## What to expect

Pierdesk is maintained by one person, so there is no guaranteed response time. The maintainer will try to acknowledge a
report within about a week, will tell you whether it is accepted, and will credit you in the release notes if you wish.
Please keep the details private until a fixed version is available, or until you and the maintainer have agreed on a date.

## Supported versions

Pierdesk is in beta. Security fixes are made in the latest release only. Please check that the problem still exists in
the [latest release](https://github.com/micky-meecky/Pierdesk/releases/latest) before reporting.

## Scope

In scope:

- pairing, device identity and the encrypted connection between paired computers;
- the clipboard and file transfer channel, including the way files are written on the receiving computer;
- the cross-screen control channel;
- Remote work: the SSH gateway, the permissions you approve for a device, and the installation of the bundled OpenSSH;
- the update and packaging scripts in this repository.

Out of scope:

- an attacker who already has administrator access or physical access to a computer that runs Pierdesk;
- problems in other products that Pierdesk works with, such as Microsoft PowerToys or Windows itself;
- findings that need you to approve a pairing or SSH access that you did not actually intend to approve;
- reports without a security impact, such as spelling mistakes (please use a normal issue for those).

Please act in good faith: test only with computers and data you own, and do not disrupt other people's networks.

---

# 安全策略

Pierdesk 会在你的网络内的电脑之间传输剪贴板、文件，以及可选的 SSH 会话，所以会认真对待安全问题。

## 报告漏洞

请**不要**为安全问题开公开的 Issue。

请在 GitHub 上私下报告：打开本仓库的 **Security** 页签，选择 **Report a vulnerability**
（[直达链接](https://github.com/micky-meecky/Pierdesk/security/advisories/new)）。只有你和维护者能看到报告内容。

有用的报告包括：Pierdesk 版本（设置 → 关于）和 Windows 版本；发现了什么、攻击者能做什么；复现步骤（有概念验证更好）；
攻击者是否需要处于同一局域网、拥有已配对设备或有本机权限。请不要附带真实的密码、密钥或他人的数据。

## 你可以期待什么

Pierdesk 由一个人维护，因此不能保证响应时间。维护者会尽量在一周左右确认收到，告知是否采纳，并在你愿意时于发布说明中致谢。
在修复版本发布之前，或在你与维护者约定的日期之前，请不要公开细节。

## 支持的版本

Pierdesk 处于 beta 阶段，安全修复只针对最新发布的版本。报告前请确认问题在
[最新版本](https://github.com/micky-meecky/Pierdesk/releases/latest) 中仍然存在。

## 范围

范围内：配对、设备身份和已配对电脑之间的加密连接；剪贴板与文件传输通道（包括接收端写入文件的方式）；跨屏控制通道；
远程工作（SSH 网关、你为设备批准的权限、内置 OpenSSH 的安装）；本仓库里的更新与打包脚本。

范围外：已经拥有管理员权限或物理接触运行 Pierdesk 的电脑的攻击者；与 Pierdesk 配合使用的其他产品（如 Microsoft PowerToys、Windows 本身）的问题；
需要你自己批准了并非本意的配对或 SSH 访问才能触发的问题；没有安全影响的问题（如错别字，请用普通 Issue）。

请本着善意行事：只用你自己的电脑和数据测试，不要干扰他人的网络。
