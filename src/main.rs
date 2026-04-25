// 基础变量和函数示例
fn main() {
    println!("=== Rust 基础示例 ===\n");

    // 1. 变量和不可变/可变
    let greeting = "Hello";
    let mut name = "World";
    println!("1. 变量示例: {} {}!", greeting, name);

    // 修改可变变量
    name = "Rust";
    println!("   修改可变变量后: {} {}!", greeting, name);

    // 2. 数据类型
    let is_rust_fun: bool = true;
    let count: i32 = 42;
    let pi: f64 = 3.14159;
    println!("\n2. 数据类型:");
    println!("   布尔值: {}, 整数: {}, 浮点数: {}", is_rust_fun, count, pi);

    // 3. 元组
    let tuple = (1, "hello", 3.14);
    let (a, b, c) = tuple;
    println!("\n3. 元组解构: ({}, {}, {})", a, b, c);
    println!("   通过索引访问: tuple.0 = {}", tuple.0);

    // 4. 数组
    let array = [1, 2, 3, 4, 5];
    println!("\n4. 数组: {:?}, 第一个元素: {}", array, array[0]);
    println!("   数组长度: {}", array.len());

    // 5. 调用函数
    println!("\n5. 函数调用:");
    let sum = add(5, 3);
    println!("   5 + 3 = {}", sum);

    // 6. 条件表达式
    println!("\n6. 条件表达式:");
    let number = 6;
    if number % 2 == 0 {
        println!("   {} 是偶数", number);
    } else {
        println!("   {} 是奇数", number);
    }

    // 7. 循环
    println!("\n7. 循环示例 (1-3):");
    for i in 1..=3 {
        println!("   循环迭代: {}", i);
    }

    // 8. Vector (动态数组)
    println!("\n8. Vector 示例:");
    let mut fruits = vec!["apple", "banana", "orange"];
    fruits.push("grape");
    println!("   {:?}", fruits);
    println!("   第一个水果: {}", fruits[0]);

    // 9. 模式匹配
    println!("\n9. match 模式匹配:");
    let day = 3;
    match day {
        1 => println!("   星期一"),
        2 => println!("   星期二"),
        3 => println!("   星期三"),
        _ => println!("   其他星期"),
    }

    // 10. 结构体
    println!("\n10. 结构体示例:");
    let user = User {
        name: "Alice".to_string(),
        age: 30,
        active: true,
    };
    println!("   用户: {}, 年龄: {}, 活跃: {}", user.name, user.age, user.active);
    user.say_hello();

    // 11. 枚举
    println!("\n11. 枚举示例:");
    let msg = Message::Text("Hello from enum".to_string());
    match msg {
        Message::Text(text) => println!("   消息内容: {}", text),
        Message::Move { x, y } => println!("   移动到: ({}, {})", x, y),
        Message::Quit => println!("   退出消息"),
    }

    println!("\n=== 示例完成 ===");
}

// 简单函数
fn add(a: i32, b: i32) -> i32 {
    a + b
}

// 结构体定义
#[derive(Debug)]
struct User {
    name: String,
    age: u8,
    active: bool,
}

// 结构体方法
impl User {
    fn say_hello(&self) {
        println!("   你好，我是 {}!", self.name);
    }
}

// 枚举定义
enum Message {
    Quit,
    Move { x: i32, y: i32 },
    Text(String),
}

