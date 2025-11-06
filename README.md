

  快速运行

  - 直接运行调试版：cargo run -p alacritty
  - 或运行发布版二进制：cargo build -p alacritty --release 后执行 ./target/release/alacritty
  - 启动后右上角菜单栏会显示 “Alacritty” 文本


cargo run -p alacritty -- -vvv

配置与分隔线

- 通过菜单栏图标 -> 配置 打开配置窗口。
- 使用“＋”添加常用目录；使用“－”删除选中项；使用“分隔线”可在选中行之后插入一条分隔线（---）。
- 右键菜单会按配置顺序显示目录与分隔线；也可拖拽行进行排序。
