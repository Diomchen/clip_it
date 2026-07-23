# ClipIt 10/25 GbE 吞吐基准

`clip-it benchmark` 使用与文件传输相同的监听端口，通过多条并发 TCP 流发送内存
缓冲区。接收端只读取和计数，不写磁盘，因此结果用于观察网络栈和链路上限；真实
文件传输还会受到 SSD、文件系统和 BLAKE3 校验速度影响。

## 推荐矩阵

| 链路 | 数据量 | 并发流 | 建议最低有效吞吐 |
|---|---:|---:|---:|
| 10 GbE | 4 GiB | 1 | 7.0 Gbit/s |
| 10 GbE | 4 GiB | 4 | 8.5 Gbit/s |
| 25 GbE | 16 GiB | 4 | 18.0 Gbit/s |
| 25 GbE | 16 GiB | 8 | 21.0 Gbit/s |

在发送端运行：

```powershell
clip-it benchmark --device RECEIVER --size-gib 4 --streams 4
clip-it benchmark --to 192.168.1.20:42490 --size-gib 16 --streams 8
```

每个组合预热一次，再记录三次正式结果的中位数。测试期间关闭 VPN、云盘同步和其他
大流量任务，并确认两端网卡协商速率、MTU 和交换机端口设置一致。Windows 电源模式
建议设为“最佳性能”；macOS 可用 `networkQuality` 和系统活动监视器排除背景流量。

## 结果记录

记录 ClipIt 版本、两端操作系统、CPU、网卡、交换机、MTU、数据量、并发流和输出的
Gbit/s。不要把 `127.0.0.1` 回环成绩当作物理网卡成绩。

开发机在 2026-07-22 使用 1 GiB、4 条流完成回环冒烟测试，结果为 76.13 Gbit/s；
该数字仅证明协议和并发调度没有在 25 Gbit/s 以下形成软件瓶颈。
