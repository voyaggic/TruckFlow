import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { ReferenceFieldsProvider } from "./lib/referenceFields";
import "./styles/theme.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ReferenceFieldsProvider>
      <App />
    </ReferenceFieldsProvider>
  </React.StrictMode>,
);
