import React from "react";
import { createRoot } from "react-dom/client";
import { FluentProvider, webDarkTheme } from "@fluentui/react-components";
import App from "./App.jsx";
import "./global.css";

createRoot(document.getElementById("root")).render(
    <React.StrictMode>
        <FluentProvider theme={webDarkTheme}>
            <App />
        </FluentProvider>
    </React.StrictMode>,
);
