# Deep Search

![Deep Search UI](assets/main%20ui.png)

A blazing fast search tool for Windows that indexes everything on your PC by directly reading the Master File Table (MFT).

> **Note:** This tool is designed exclusively for **Windows** (NTFS drives).

![Demo](demo.gif)

## Features

*   **Instant Indexing:** Indexes files live as soon as the app opens.
*   **MFT Parsing:** Reads the raw Master File Table for maximum speed.
*   **No Background Services:** Unlike other search tools, this doesn't run a heavy background indexer that slows down your PC.
*   **See Everything:** Shows literally every file on your system, including hidden and system files.
*   **Multi-Drive Support:** Automatically detects and scans all connected NTFS drives.

## Installation

### Option 1: Download Installer (Recommended)
You can simply download the latest `Deep Search Setup.exe` from the [Releases](https://github.com/basilbenny1002/Deep-Search/releases) page and install it on your system.

### Option 2: Build from Source
If you prefer to build it yourself, you will need [Rust](https://www.rust-lang.org/tools/install) installed.

1.  Open your terminal as **Administrator** (Required for MFT access).
2.  Clone the repository:
    ```bash
    git clone https://github.com/basilbenny1002/Deep-Search.git
    cd Deep-Search
    ```
3.  Build and run the application:
    ```bash
    cargo run --release
    ```
    *Cargo will automatically download and compile all necessary dependencies.*

## Usage

1.  **Launch the App:** Open `Deep Search` from your Start Menu or run it via terminal.
2.  **Wait for Indexing:** Give it a few seconds to scan all your drives. The time depends on the number of files and drives you have.
3.  **Search:** Once the scan is complete, the search bar will appear. Type to filter results instantly.
4.  **Open Files:** Click on any result to open its location in Windows Explorer with the file selected/highlighted.

## Project Structure

```
deep_search/
├── assets/             # Icons and UI images
├── src/
│   └── main.rs         # Core logic (UI, MFT parsing, Threading)
├── build.rs            # Build script for Admin Manifest & Icons
└── Cargo.toml          # Dependencies
```

## Contributing

This project is still in active development! Feel free to open issues, submit pull requests, or share feedback on how to improve it.

## Contact

**Basil Benny**

- Email: [basilbenny1002@gmail.com](mailto:basilbenny1002@gmail.com)
- LinkedIn: [basil-benny12](https://www.linkedin.com/in/basil-benny12/)
- Instagram: [@basil_benny12](https://www.instagram.com/basil_benny12)
- GitHub: [@basilbenny1002](https://github.com/basilbenny1002)

For help, support, or questions, feel free to reach out on any of these platforms.

## Resources

- **Learn How It Works:** [Medium Article](https://medium.com/@basilbenny1002/from-trees-to-sqlite-my-journey-building-a-smarter-search-for-windows-d85d77275c68)

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
